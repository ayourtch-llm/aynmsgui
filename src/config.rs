use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Cisco network management web GUI")]
pub struct AppConfig {
    /// IP address to listen on
    #[arg(long, default_value = "::", env = "AYNMSGUI_LISTEN_ADDR")]
    pub listen_addr: String,

    /// Port to listen on
    #[arg(long, default_value_t = 8080, env = "AYNMSGUI_PORT")]
    pub port: u16,

    /// Path to htpasswd file for authentication
    #[arg(long, default_value = "data/htpasswd", env = "AYNMSGUI_HTPASSWD_FILE")]
    pub htpasswd_file: PathBuf,

    /// Session TTL in seconds
    #[arg(long, default_value_t = 86400, env = "AYNMSGUI_SESSION_TTL_SECS")]
    pub session_ttl_secs: u64,

    /// Path to JSONL inventory file for AssetCache
    #[arg(long, default_value = "data/inventory.jsonl", env = "AYNMSGUI_INVENTORY_PATH")]
    pub inventory_path: Option<PathBuf>,

    /// Comma-delimited list of address map URLs
    #[arg(
        long,
        value_delimiter = ',',
        env = "AYNMSGUI_ADDRESS_MAP_URLS"
    )]
    pub address_map_urls: Vec<String>,

    /// Interval in seconds between address map refreshes
    #[arg(
        long,
        default_value_t = 60,
        env = "AYNMSGUI_ADDRESS_MAP_REFRESH_SECS"
    )]
    pub address_map_refresh_secs: u64,

    /// Base directory for config generation
    #[arg(long, default_value = "data/cfggen", env = "AYNMSGUI_CFGGEN_BASE_DIR")]
    pub cfggen_base_dir: Option<PathBuf>,

    /// Path to target configs
    #[arg(long, default_value = "data/target-configs", env = "AYNMSGUI_TARGET_CONFIGS_PATH")]
    pub target_configs_path: Option<PathBuf>,

    /// Path to current configs
    #[arg(long, default_value = "data/current-configs", env = "AYNMSGUI_CURRENT_CONFIGS_PATH")]
    pub current_configs_path: Option<PathBuf>,

    /// Target branch name
    #[arg(long, default_value = "main", env = "AYNMSGUI_TARGET_BRANCH")]
    pub target_branch: String,

    /// Current branch name
    #[arg(long, default_value = "main", env = "AYNMSGUI_CURRENT_BRANCH")]
    pub current_branch: String,

    /// Username for device authentication
    #[arg(long, env = "AYNMSGUI_DEVICE_USERNAME")]
    pub device_username: Option<String>,

    /// Password for device authentication
    #[arg(long, env = "AYNMSGUI_DEVICE_PASSWORD")]
    pub device_password: Option<String>,

    /// Directory for images
    #[arg(long, default_value = "data/images", env = "AYNMSGUI_IMAGES_DIR")]
    pub images_dir: Option<PathBuf>,

    /// Path to assignments JSON file
    #[arg(
        long,
        default_value = "data/assignments.json",
        env = "AYNMSGUI_ASSIGNMENTS_FILE"
    )]
    pub assignments_file: PathBuf,

    /// Directory for changes
    #[arg(long, default_value = "data/changes", env = "AYNMSGUI_CHANGES_DIR")]
    pub changes_dir: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse AppConfig from a slice of CLI arguments (including the program name).
    fn parse_args(args: &[&str]) -> Result<AppConfig, clap::Error> {
        AppConfig::try_parse_from(args)
    }

    #[test]
    fn test_defaults_parse_correctly() {
        let cfg = parse_args(&["aynmsgui"])
            .expect("should parse with all defaults");

        assert_eq!(cfg.listen_addr, "::");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.htpasswd_file, PathBuf::from("data/htpasswd"));
        assert_eq!(cfg.session_ttl_secs, 86400);
        assert_eq!(cfg.inventory_path, Some(PathBuf::from("data/inventory.jsonl")));
        assert!(cfg.address_map_urls.is_empty());
        assert_eq!(cfg.address_map_refresh_secs, 60);
        assert_eq!(cfg.cfggen_base_dir, Some(PathBuf::from("data/cfggen")));
        assert_eq!(cfg.target_configs_path, Some(PathBuf::from("data/target-configs")));
        assert_eq!(cfg.current_configs_path, Some(PathBuf::from("data/current-configs")));
        assert_eq!(cfg.target_branch, "main");
        assert_eq!(cfg.current_branch, "main");
        assert!(cfg.device_username.is_none());
        assert!(cfg.device_password.is_none());
        assert_eq!(cfg.images_dir, Some(PathBuf::from("data/images")));
        assert_eq!(cfg.assignments_file, PathBuf::from("data/assignments.json"));
        assert_eq!(cfg.changes_dir, Some(PathBuf::from("data/changes")));
    }

    #[test]
    fn test_custom_values_parse_correctly() {
        let cfg = parse_args(&[
            "aynmsgui",
            "--htpasswd-file", "/custom/htpasswd",
            "--listen-addr", "0.0.0.0",
            "--port", "9090",
            "--session-ttl-secs", "3600",
            "--target-branch", "develop",
            "--current-branch", "feature-x",
            "--address-map-urls", "http://a.example.com,http://b.example.com",
            "--address-map-refresh-secs", "120",
            "--assignments-file", "my-assignments.json",
        ])
        .expect("should parse all custom values");

        assert_eq!(cfg.listen_addr, "0.0.0.0");
        assert_eq!(cfg.port, 9090);
        assert_eq!(cfg.htpasswd_file, PathBuf::from("/custom/htpasswd"));
        assert_eq!(cfg.session_ttl_secs, 3600);
        assert_eq!(cfg.target_branch, "develop");
        assert_eq!(cfg.current_branch, "feature-x");
        assert_eq!(
            cfg.address_map_urls,
            vec!["http://a.example.com", "http://b.example.com"]
        );
        assert_eq!(cfg.address_map_refresh_secs, 120);
        assert_eq!(cfg.assignments_file, PathBuf::from("my-assignments.json"));
    }

    #[test]
    fn test_optional_paths_parse_correctly() {
        let cfg = parse_args(&[
            "aynmsgui",
            "--htpasswd-file", "/etc/htpasswd",
            "--inventory-path", "/var/inventory.jsonl",
            "--cfggen-base-dir", "/opt/cfggen",
            "--target-configs-path", "/opt/target",
            "--current-configs-path", "/opt/current",
            "--images-dir", "/var/images",
            "--changes-dir", "/var/changes",
            "--device-username", "admin",
            "--device-password", "s3cr3t",
        ])
        .expect("should parse optional paths");

        assert_eq!(cfg.inventory_path, Some(PathBuf::from("/var/inventory.jsonl")));
        assert_eq!(cfg.cfggen_base_dir, Some(PathBuf::from("/opt/cfggen")));
        assert_eq!(cfg.target_configs_path, Some(PathBuf::from("/opt/target")));
        assert_eq!(cfg.current_configs_path, Some(PathBuf::from("/opt/current")));
        assert_eq!(cfg.images_dir, Some(PathBuf::from("/var/images")));
        assert_eq!(cfg.changes_dir, Some(PathBuf::from("/var/changes")));
        assert_eq!(cfg.device_username, Some("admin".to_string()));
        assert_eq!(cfg.device_password, Some("s3cr3t".to_string()));
    }
}
