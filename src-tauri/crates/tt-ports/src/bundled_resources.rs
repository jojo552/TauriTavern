use tt_domain::errors::DomainError;

/// Read-only resources supplied by the host, independent of their packaging.
///
/// These synchronous operations may perform blocking IO. Async callers must run
/// them on a blocking executor.
pub trait BundledResources: Send + Sync {
    /// Read a path relative to the resource root, such as
    /// `default/content/settings.json`. Missing resources return `NotFound`;
    /// other failures must not be replaced with empty bytes or another source.
    fn read_bytes(&self, relative_path: &str) -> Result<Vec<u8>, DomainError>;

    /// List files recursively beneath a prefix in `default/<root>` (`content` or `skills`).
    /// Results are sorted, slash-separated paths relative to that root, including
    /// the prefix itself (for example `sprites/Alice/happy.png` under `content`).
    /// An empty prefix lists all files in the root. A prefix with no files returns
    /// an empty list; manifest or IO failures must be propagated.
    fn list_default_files_under(
        &self,
        root: &str,
        prefix: &str,
    ) -> Result<Vec<String>, DomainError>;
}
