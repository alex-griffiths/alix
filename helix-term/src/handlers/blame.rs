use std::sync::Arc;

use helix_event::register_hook;
use helix_loader::workspace_trust::TrustQuery;
use helix_view::{
    events::{ConfigDidChange, DocumentDidOpen},
    handlers::Handlers,
    DocumentId, Editor,
};

use crate::job;

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        request_blame(event.editor, event.doc);
        Ok(())
    });

    // Workspace trust changes are followed by a config refresh, so this also picks up documents
    // which became (un)trusted.
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        let config_changed = event.old.inline_blame != event.new.inline_blame;
        let docs: Vec<_> = event.editor.documents.keys().copied().collect();
        for doc in docs {
            let trusted = blame_trusted(event.editor, doc);
            let has_blame = event.editor.documents[&doc].has_blame();
            if config_changed || trusted != has_blame {
                request_blame(event.editor, doc);
            }
        }
        Ok(())
    });
}

/// Whether the git/jj CLI may be run for the document's workspace. Both read repository local
/// configuration which can execute arbitrary commands.
pub fn blame_trusted(editor: &Editor, doc_id: DocumentId) -> bool {
    editor.documents.get(&doc_id).is_some_and(|doc| {
        editor
            .workspace_trust
            .query(doc.workspace_root(), TrustQuery::Git)
            .is_trusted()
    })
}

/// (Re)computes the blame information of a document in the background. Clears the blame
/// information if inline blame is disabled.
pub fn request_blame(editor: &mut Editor, doc_id: DocumentId) {
    let config = editor.config().inline_blame.clone();
    let trusted = blame_trusted(editor, doc_id);
    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };
    let Some(path) = doc.path().map(|path| path.to_path_buf()) else {
        return;
    };
    if !config.enable || !trusted {
        doc.clear_blame();
        return;
    }

    let request = doc.start_blame_request();
    let backend = config.backend.into();
    tokio::task::spawn_blocking(move || {
        let blame = match helix_vcs::blame_file(&path, backend) {
            Ok(blame) => Some(Arc::new(blame)),
            Err(err) => {
                log::debug!("failed to blame {}: {err:#}", path.display());
                None
            }
        };
        job::dispatch_blocking(move |editor, _| {
            if let Some(doc) = editor.documents.get_mut(&doc_id) {
                doc.set_blame(request, blame);
            }
        });
    });
}

/// Refreshes the blame information of all visible documents, for example because commits
/// might have been created outside of helix.
pub fn refresh_visible(editor: &mut Editor) {
    if !editor.config().inline_blame.enable {
        return;
    }
    let mut docs: Vec<_> = editor.tree.views().map(|(view, _)| view.doc).collect();
    docs.sort_unstable();
    docs.dedup();
    for doc in docs {
        request_blame(editor, doc);
    }
}
