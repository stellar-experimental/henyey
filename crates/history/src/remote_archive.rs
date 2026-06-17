//! Remote archive operations using configurable shell commands.
//!
//! This module provides the ability to upload files to remote history archives
//! using configurable shell commands, matching the stellar-core behavior.
//!
//! # Command Templates
//!
//! Commands use placeholder syntax:
//! - `{0}` - First argument (typically local file or remote path)
//! - `{1}` - Second argument (typically remote path or local file)
//!
//! # Example Configuration
//!
//! ```toml
//! [[history.archives]]
//! name = "s3-archive"
//! get = "curl -sf {0} -o {1}"
//! put = "aws s3 cp {0} s3://my-bucket{1} --region us-east-1"
//! mkdir = "aws s3api put-object --bucket my-bucket --key {0}/"
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use henyey_history::remote_archive::{RemoteArchive, RemoteArchiveConfig};
//! use std::path::Path;
//!
//! let config = RemoteArchiveConfig {
//!     name: "s3-archive".to_string(),
//!     put_cmd: Some("aws s3 cp {0} s3://my-bucket{1}".to_string()),
//!     mkdir_cmd: Some("aws s3api put-object --bucket my-bucket --key {0}/".to_string()),
//!     ..Default::default()
//! };
//!
//! let archive = RemoteArchive::new(config);
//!
//! # async fn example(archive: RemoteArchive) -> Result<(), henyey_history::HistoryError> {
//! // Create directory
//! archive.mkdir("/history/00/00/00").await?;
//!
//! // Upload file
//! archive.put_file(Path::new("/local/path/file.xdr.gz"), "/history/00/00/00/file.xdr.gz").await?;
//! # Ok(())
//! # }
//! ```

use crate::{HistoryError, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

/// Configuration for a remote history archive.
#[derive(Debug, Clone, Default)]
pub struct RemoteArchiveConfig {
    /// Name of the archive (for logging/identification).
    pub name: String,
    /// Command template for downloading files.
    /// Placeholders: {0} = remote URL, {1} = local path.
    pub get_cmd: Option<String>,
    /// Command template for uploading files.
    /// Placeholders: {0} = local path, {1} = remote path.
    pub put_cmd: Option<String>,
    /// Command template for creating remote directories.
    /// Placeholders: {0} = remote directory path.
    pub mkdir_cmd: Option<String>,
}

impl RemoteArchiveConfig {
    /// Check if this archive is writable (has put command configured).
    pub fn is_writable(&self) -> bool {
        self.put_cmd.is_some()
    }

    /// Check if this archive is readable (has get command configured).
    pub fn is_readable(&self) -> bool {
        self.get_cmd.is_some()
    }
}

/// A remote history archive that uses shell commands for operations.
#[derive(Debug, Clone)]
pub struct RemoteArchive {
    config: RemoteArchiveConfig,
}

impl RemoteArchive {
    /// Create a new remote archive with the given configuration.
    pub fn new(config: RemoteArchiveConfig) -> Self {
        Self { config }
    }

    /// Get the archive name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Check if this archive supports uploads.
    pub fn can_write(&self) -> bool {
        self.config.is_writable()
    }

    /// Check if this archive supports downloads.
    pub fn can_read(&self) -> bool {
        self.config.is_readable()
    }

    /// Check if this archive has a mkdir command configured.
    ///
    /// Callers that want to skip mkdir entirely when no command is set
    /// (rather than receiving `HistoryError::RemoteNotConfigured` from
    /// [`Self::mkdir`]) should gate on this accessor.
    pub fn has_mkdir(&self) -> bool {
        self.config.mkdir_cmd.is_some()
    }

    /// Format a command template by replacing placeholders.
    ///
    /// Placeholders are `{0}`, `{1}`, etc.
    fn format_command(template: &str, args: &[&str]) -> String {
        let mut result = template.to_string();
        for (i, arg) in args.iter().enumerate() {
            let placeholder = format!("{{{}}}", i);
            result = result.replace(&placeholder, arg);
        }
        result
    }

    /// Execute a shell command and return success/failure.
    async fn execute_command(&self, command: &str) -> Result<()> {
        debug!(archive = %self.config.name, command = %command, "Executing remote archive command");

        // Use shell to execute the command
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(HistoryError::Io)?;

        if output.status.success() {
            debug!(archive = %self.config.name, "Command succeeded");
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            warn!(
                archive = %self.config.name,
                command = %command,
                exit_code = ?output.status.code(),
                stderr = %stderr,
                stdout = %stdout,
                "Remote archive command failed"
            );
            Err(HistoryError::RemoteCommandFailed {
                command: command.to_string(),
                exit_code: output.status.code(),
                stderr: stderr.to_string(),
            })
        }
    }

    /// Upload a local file to a remote path.
    ///
    /// # Arguments
    ///
    /// * `local_path` - Path to the local file to upload
    /// * `remote_path` - Destination path in the archive
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No put command is configured
    /// - The local file doesn't exist
    /// - The command execution fails
    pub async fn put_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let put_cmd = self.config.put_cmd.as_ref().ok_or_else(|| {
            HistoryError::RemoteNotConfigured("put command not configured".to_string())
        })?;

        if !local_path.exists() {
            return Err(HistoryError::NotFound(format!(
                "Local file not found: {}",
                local_path.display()
            )));
        }

        let local_str = local_path.to_string_lossy();
        let command = Self::format_command(put_cmd, &[&local_str, remote_path]);

        self.execute_command(&command).await
    }

    /// Create a remote directory.
    ///
    /// # Arguments
    ///
    /// * `remote_dir` - Path of the directory to create
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No mkdir command is configured
    /// - The command execution fails
    pub async fn mkdir(&self, remote_dir: &str) -> Result<()> {
        let mkdir_cmd = self.config.mkdir_cmd.as_ref().ok_or_else(|| {
            HistoryError::RemoteNotConfigured("mkdir command not configured".to_string())
        })?;

        let command = Self::format_command(mkdir_cmd, &[remote_dir]);

        self.execute_command(&command).await
    }

    /// Ensure a remote directory exists, creating it if necessary.
    ///
    /// This is a no-op if no mkdir command is configured.
    pub async fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        if self.config.mkdir_cmd.is_some() {
            // Ignore errors from mkdir as the directory may already exist
            let _ = self.mkdir(remote_dir).await;
        }
        Ok(())
    }

    /// Upload a file, creating parent directories as needed.
    ///
    /// # Arguments
    ///
    /// * `local_path` - Path to the local file to upload
    /// * `remote_path` - Destination path in the archive
    ///
    /// # Errors
    ///
    /// Returns an error if the upload fails.
    pub async fn put_file_with_mkdir(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        // Extract parent directory from remote path
        if let Some(parent) = Path::new(remote_path).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() && parent_str != "/" {
                self.ensure_dir(&parent_str).await?;
            }
        }

        self.put_file(local_path, remote_path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_command_single_arg() {
        let template = "mkdir -p {0}";
        let result = RemoteArchive::format_command(template, &["/path/to/dir"]);
        assert_eq!(result, "mkdir -p /path/to/dir");
    }

    #[test]
    fn test_format_command_two_args() {
        let template = "cp {0} {1}";
        let result = RemoteArchive::format_command(template, &["/local/file", "/remote/file"]);
        assert_eq!(result, "cp /local/file /remote/file");
    }

    #[test]
    fn test_format_command_repeated_placeholder() {
        let template = "echo {0} {0} {1}";
        let result = RemoteArchive::format_command(template, &["hello", "world"]);
        assert_eq!(result, "echo hello hello world");
    }

    #[test]
    fn test_format_command_aws_s3() {
        let template = "aws s3 cp {0} s3://my-bucket{1} --region us-east-1";
        let result = RemoteArchive::format_command(
            template,
            &["/local/file.xdr.gz", "/history/00/00/00/file.xdr.gz"],
        );
        assert_eq!(
            result,
            "aws s3 cp /local/file.xdr.gz s3://my-bucket/history/00/00/00/file.xdr.gz --region us-east-1"
        );
    }

    #[test]
    fn test_config_is_writable() {
        let mut config = RemoteArchiveConfig::default();
        assert!(!config.is_writable());

        config.put_cmd = Some("cp {0} {1}".to_string());
        assert!(config.is_writable());
    }

    #[test]
    fn test_config_is_readable() {
        let mut config = RemoteArchiveConfig::default();
        assert!(!config.is_readable());

        config.get_cmd = Some("curl {0} -o {1}".to_string());
        assert!(config.is_readable());
    }

    #[tokio::test]
    async fn test_put_file_no_command() {
        let config = RemoteArchiveConfig::default();
        let archive = RemoteArchive::new(config);

        let result = archive
            .put_file(Path::new("/nonexistent"), "/remote/path")
            .await;
        assert!(matches!(result, Err(HistoryError::RemoteNotConfigured(_))));
    }

    #[tokio::test]
    async fn test_mkdir_no_command() {
        let config = RemoteArchiveConfig::default();
        let archive = RemoteArchive::new(config);

        let result = archive.mkdir("/remote/dir").await;
        assert!(matches!(result, Err(HistoryError::RemoteNotConfigured(_))));
    }

    #[tokio::test]
    async fn test_put_file_local_not_found() {
        let config = RemoteArchiveConfig {
            put_cmd: Some("cp {0} {1}".to_string()),
            ..Default::default()
        };
        let archive = RemoteArchive::new(config);

        let result = archive
            .put_file(Path::new("/definitely/does/not/exist"), "/remote/path")
            .await;
        assert!(matches!(result, Err(HistoryError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_execute_command_success() {
        let config = RemoteArchiveConfig {
            name: "test".to_string(),
            put_cmd: Some("echo {0} {1}".to_string()),
            ..Default::default()
        };
        let archive = RemoteArchive::new(config);

        // Create a temp file
        let temp = tempfile::NamedTempFile::new().unwrap();

        let result = archive.put_file(temp.path(), "/remote/path").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_command_failure() {
        let config = RemoteArchiveConfig {
            name: "test".to_string(),
            put_cmd: Some("exit 1".to_string()),
            ..Default::default()
        };
        let archive = RemoteArchive::new(config);

        // Create a temp file
        let temp = tempfile::NamedTempFile::new().unwrap();

        let result = archive.put_file(temp.path(), "/remote/path").await;
        assert!(matches!(
            result,
            Err(HistoryError::RemoteCommandFailed { .. })
        ));
    }

    #[tokio::test]
    async fn test_mkdir_success() {
        let config = RemoteArchiveConfig {
            name: "test".to_string(),
            mkdir_cmd: Some("echo creating {0}".to_string()),
            ..Default::default()
        };
        let archive = RemoteArchive::new(config);

        let result = archive.mkdir("/remote/dir").await;
        assert!(result.is_ok());
    }
}
