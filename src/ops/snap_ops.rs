use crate::protocol::{Op, Response};
use crate::session::Session;
use serde_json::json;

/// Opcode 12: Create a workspace snapshot.
/// p = optional tag name
pub async fn snap(session: &Session, op: &Op) -> Response {
    let tag = op.p.clone();
    match session.snapshot_store.create(tag).await {
        Ok(meta) => Response::ok(serde_json::to_value(meta).unwrap()),
        Err(e) => Response::err(4, &e),
    }
}

/// Opcode 13: Restore a workspace snapshot.
/// p = snapshot id (required)
pub async fn restore(session: &Session, op: &Op) -> Response {
    let id = match &op.p {
        Some(id) => id.as_str(),
        None => return Response::err(3, "restore requires snapshot id in 'p'"),
    };

    match session.snapshot_store.restore(id).await {
        Ok(meta) => Response::ok(serde_json::to_value(meta).unwrap()),
        Err(e) => Response::err(7, &e),
    }
}

/// Opcode 25: List all snapshots.
pub async fn snap_list(session: &Session) -> Response {
    match session.snapshot_store.list() {
        Ok(snaps) => Response::ok(json!({"snapshots": snaps})),
        Err(e) => Response::err(4, &e),
    }
}

/// Opcode 26: Delete a snapshot.
/// p = snapshot id (required)
pub async fn snap_delete(session: &Session, op: &Op) -> Response {
    let id = match &op.p {
        Some(id) => id.as_str(),
        None => return Response::err(3, "snap_delete requires snapshot id in 'p'"),
    };

    match session.snapshot_store.delete(id) {
        Ok(()) => Response::ok(json!({"deleted": id})),
        Err(e) => Response::err(7, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    #[tokio::test]
    async fn snap_and_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "original").unwrap();

        let s = session_in(dir.path());

        let r = snap(&s, &Op {
            c: 12,
            p: Some("checkpoint".into()),
            ..Op::default()
        }).await;
        assert!(r.ok);
        let id = r.d.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        std::fs::write(dir.path().join("file.txt"), "modified").unwrap();
        std::fs::write(dir.path().join("extra.txt"), "new file").unwrap();

        let r = restore(&s, &Op {
            c: 13,
            p: Some(id.clone()),
            ..Op::default()
        }).await;
        assert!(r.ok);

        assert_eq!(std::fs::read_to_string(dir.path().join("file.txt")).unwrap(), "original");
        assert!(!dir.path().join("extra.txt").exists());
    }

    #[tokio::test]
    async fn snap_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = snap_list(&s).await;
        assert!(r.ok);
        assert_eq!(r.d.as_ref().unwrap()["snapshots"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn snap_list_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let s = session_in(dir.path());

        snap(&s, &Op { c: 12, p: Some("a".into()), ..Op::default() }).await;
        snap(&s, &Op { c: 12, p: Some("b".into()), ..Op::default() }).await;

        let r = snap_list(&s).await;
        assert!(r.ok);
        assert_eq!(r.d.as_ref().unwrap()["snapshots"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn snap_delete_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let s = session_in(dir.path());

        let r = snap(&s, &Op { c: 12, ..Op::default() }).await;
        let id = r.d.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let r = snap_delete(&s, &Op { c: 26, p: Some(id), ..Op::default() }).await;
        assert!(r.ok);

        let r = snap_list(&s).await;
        assert_eq!(r.d.as_ref().unwrap()["snapshots"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn snap_delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = snap_delete(&s, &Op { c: 26, p: Some("nope".into()), ..Op::default() }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(7));
    }

    #[tokio::test]
    async fn restore_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = restore(&s, &Op { c: 13, p: Some("nope".into()), ..Op::default() }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(7));
    }

    #[tokio::test]
    async fn restore_missing_id() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_in(dir.path());

        let r = restore(&s, &Op { c: 13, ..Op::default() }).await;
        assert!(!r.ok);
        assert_eq!(r.e, Some(3));
    }
}
