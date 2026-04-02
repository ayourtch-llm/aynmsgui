// TODO: add this field to AppState:
//   pub assignments: Arc<tokio::sync::RwLock<crate::assignments::AssignmentMap>>

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentMap {
    /// serial_number -> logical_device_name
    assignments: IndexMap<String, String>,
    /// File path for persistence (not serialized)
    #[serde(skip)]
    file_path: Option<PathBuf>,
}

impl AssignmentMap {
    pub fn new() -> Self {
        Self {
            assignments: IndexMap::new(),
            file_path: None,
        }
    }

    /// Load from a JSON file. If the file does not exist, return an empty map (not an error).
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            warn!(path = %path.display(), "Assignment file not found, starting with empty map");
            return Ok(Self {
                assignments: IndexMap::new(),
                file_path: Some(path.to_path_buf()),
            });
        }

        let contents = std::fs::read_to_string(path)?;
        let mut map: AssignmentMap = serde_json::from_str(&contents)?;
        map.file_path = Some(path.to_path_buf());
        info!(path = %path.display(), count = map.assignments.len(), "Loaded assignments from file");
        Ok(map)
    }

    /// Save to the configured file path. No-op if no file path is set.
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(ref path) = self.file_path else {
            warn!("No file path set for AssignmentMap, skipping save");
            return Ok(());
        };
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        info!(path = %path.display(), "Saved assignments to file");
        Ok(())
    }

    /// Assign a serial number to a logical device name.
    ///
    /// Enforces a 1:1 constraint:
    /// - A serial may not be reassigned to a different device.
    /// - A device may not receive a second serial.
    /// - Assigning the same serial to the same device is idempotent (OK).
    pub fn assign(&mut self, serial: &str, device_name: &str) -> Result<(), String> {
        // Check if serial already has an assignment.
        if let Some(existing_device) = self.assignments.get(serial) {
            if existing_device == device_name {
                // Idempotent — already assigned to the same device.
                return Ok(());
            }
            return Err(format!(
                "Serial '{}' is already assigned to device '{}'",
                serial, existing_device
            ));
        }

        // Check if device_name is already taken by another serial.
        if let Some(existing_serial) = self.get_serial_for_device(device_name) {
            return Err(format!(
                "Device '{}' is already assigned to serial '{}'",
                device_name, existing_serial
            ));
        }

        self.assignments
            .insert(serial.to_string(), device_name.to_string());
        info!(serial, device_name, "Assigned serial to device");
        Ok(())
    }

    /// Remove the assignment for a serial number (no-op if not assigned).
    pub fn unassign(&mut self, serial: &str) {
        if self.assignments.shift_remove(serial).is_some() {
            info!(serial, "Unassigned serial");
        }
    }

    /// Return the logical device name for a given serial, if any.
    pub fn get_device_for_serial(&self, serial: &str) -> Option<&str> {
        self.assignments.get(serial).map(String::as_str)
    }

    /// Reverse lookup: return the serial for a given device name, if any.
    pub fn get_serial_for_device(&self, device_name: &str) -> Option<&str> {
        self.assignments
            .iter()
            .find(|(_, v)| v.as_str() == device_name)
            .map(|(k, _)| k.as_str())
    }

    /// Return the full assignment map.
    pub fn all_assignments(&self) -> &IndexMap<String, String> {
        &self.assignments
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let map = AssignmentMap::new();
        assert!(map.all_assignments().is_empty());
    }

    #[test]
    fn test_assign_and_lookup() {
        let mut map = AssignmentMap::new();
        map.assign("SN-001", "router-a").unwrap();

        assert_eq!(map.get_device_for_serial("SN-001"), Some("router-a"));
        assert_eq!(map.get_serial_for_device("router-a"), Some("SN-001"));
        // Non-existent lookups.
        assert_eq!(map.get_device_for_serial("SN-999"), None);
        assert_eq!(map.get_serial_for_device("router-z"), None);
    }

    #[test]
    fn test_assign_duplicate_serial_errors() {
        let mut map = AssignmentMap::new();
        map.assign("SN-001", "router-a").unwrap();
        let err = map.assign("SN-001", "router-b").unwrap_err();
        assert!(
            err.contains("SN-001"),
            "Error should mention the serial: {err}"
        );
    }

    #[test]
    fn test_assign_duplicate_device_errors() {
        let mut map = AssignmentMap::new();
        map.assign("SN-001", "router-a").unwrap();
        let err = map.assign("SN-002", "router-a").unwrap_err();
        assert!(
            err.contains("router-a"),
            "Error should mention the device: {err}"
        );
    }

    #[test]
    fn test_assign_idempotent() {
        let mut map = AssignmentMap::new();
        map.assign("SN-001", "router-a").unwrap();
        // Same pair again — should be OK.
        map.assign("SN-001", "router-a").unwrap();
        assert_eq!(map.all_assignments().len(), 1);
    }

    #[test]
    fn test_unassign() {
        let mut map = AssignmentMap::new();
        map.assign("SN-001", "router-a").unwrap();
        map.unassign("SN-001");
        assert_eq!(map.get_device_for_serial("SN-001"), None);
        assert_eq!(map.get_serial_for_device("router-a"), None);
        // Unassigning again is a no-op.
        map.unassign("SN-001");
    }

    #[test]
    fn test_save_and_load() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("aynmsgui_test_assignments_{}.json", std::process::id()));

        let mut map = AssignmentMap::from_file(&path).unwrap();
        map.assign("SN-001", "router-a").unwrap();
        map.assign("SN-002", "router-b").unwrap();
        map.save().unwrap();

        let loaded = AssignmentMap::from_file(&path).unwrap();
        assert_eq!(loaded.get_device_for_serial("SN-001"), Some("router-a"));
        assert_eq!(loaded.get_device_for_serial("SN-002"), Some("router-b"));
        assert_eq!(loaded.all_assignments().len(), 2);
    }

    #[test]
    fn test_from_file_missing_returns_empty() {
        let dir = std::env::temp_dir();
        let path = dir.join("aynmsgui_test_does_not_exist_xyz987.json");

        let map = AssignmentMap::from_file(&path).unwrap();
        assert!(map.all_assignments().is_empty());
    }
}
