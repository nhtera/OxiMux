//! `SimApprovalRepo` — the iOS Simulator devices (by udid) the user has let
//! agents control. Written only by the desktop's consent banner and revoked in
//! Settings; see `V031__sim_device_approvals.sql` for why this is not in the
//! settings file, and what that does and does not protect against.

use rusqlite::params;

use crate::db::Db;
use crate::error::StorageError;
use crate::repositories::now;

/// One approved device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimApproval {
    pub udid: String,
    /// The device's name when it was approved, for the Settings list.
    pub device_name: String,
    /// RFC 3339.
    pub granted_at: String,
}

#[derive(Clone)]
pub struct SimApprovalRepo {
    db: Db,
}

impl SimApprovalRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Every approved device, oldest first.
    pub fn list(&self) -> Result<Vec<SimApproval>, StorageError> {
        self.db.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT udid, device_name, granted_at FROM sim_device_approvals ORDER BY granted_at")?;
            let rows = stmt.query_map([], |row| {
                Ok(SimApproval { udid: row.get(0)?, device_name: row.get(1)?, granted_at: row.get(2)? })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Approve `udid`. Approving it again refreshes the name and time.
    pub fn grant(&self, udid: &str, device_name: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO sim_device_approvals (udid, device_name, granted_at, granted_by) \
                 VALUES (?1, ?2, ?3, 'user') \
                 ON CONFLICT(udid) DO UPDATE SET device_name = excluded.device_name, granted_at = excluded.granted_at",
                params![udid, device_name, ts],
            )
            .map(|_| ())
        })
    }

    /// Withdraw the approval for `udid`. No-op when there is none.
    pub fn revoke(&self, udid: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM sim_device_approvals WHERE udid = ?1", params![udid]).map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_list_and_revoke() {
        let repo = SimApprovalRepo::new(crate::db::open_memory().expect("db"));
        assert!(repo.list().unwrap().is_empty());
        repo.grant("U-1", "iPhone 17 Pro").unwrap();
        repo.grant("U-2", "iPad Air").unwrap();
        // A second grant renames rather than duplicating.
        repo.grant("U-1", "iPhone 17 Pro (2)").unwrap();
        let rows = repo.list().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.udid == "U-1" && r.device_name == "iPhone 17 Pro (2)"));
        repo.revoke("U-1").unwrap();
        repo.revoke("U-missing").unwrap();
        assert_eq!(repo.list().unwrap().iter().map(|r| r.udid.as_str()).collect::<Vec<_>>(), ["U-2"]);
    }
}
