use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 2000;

#[derive(Debug)]
pub struct FileError {
    pub status: u16,
    pub message: &'static str,
}

impl FileError {
    fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }
}

impl From<io::Error> for FileError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::new(404, "文件或目录不存在"),
            io::ErrorKind::PermissionDenied => Self::new(403, "没有访问该文件或目录的权限"),
            _ => Self::new(500, "文件操作失败，请检查 Agent 的目录权限和磁盘状态"),
        }
    }
}

type Result<T> = std::result::Result<T, FileError>;

pub enum FileRequest {
    Config,
    List(String),
    Read(String),
    Write,
}

pub fn is_file_request(path: &str) -> bool {
    path == "/files" || path.starts_with("/files/") || path.starts_with("/files?")
}

pub fn parse_request(method: &str, path: &str) -> Result<FileRequest> {
    let (endpoint, query) = path.split_once('?').unwrap_or((path, ""));
    let mut relative = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key != "path" || relative.is_some() {
            return Err(FileError::new(400, "无效的文件请求参数"));
        }
        relative = Some(value.into_owned());
    }
    match (method, endpoint) {
        ("GET", "/files/config") if relative.is_none() => Ok(FileRequest::Config),
        ("GET", "/files/list") => {
            let path = relative.unwrap_or_default();
            validate_relative(&path, true)?;
            Ok(FileRequest::List(path))
        }
        ("GET", "/files/read") => {
            let path = relative.unwrap_or_default();
            validate_markdown(&path)?;
            Ok(FileRequest::Read(path))
        }
        ("PUT", "/files/write") if relative.is_none() => Ok(FileRequest::Write),
        _ => Err(FileError::new(403, "文件接口或请求方法不允许")),
    }
}

// Use a portable subset of file names: excludes Windows drive/ADS paths,
// device names, hidden components, and separators that vary by platform.
fn validate_relative(path: &str, allow_root: bool) -> Result<()> {
    if path.is_empty() && allow_root {
        return Ok(());
    }
    if path.is_empty()
        || path.len() > 2048
        || path.contains(['\\', ':', '%'])
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| {
            let stem = part
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            part.is_empty()
                || part.starts_with('.')
                || part.ends_with(['.', ' '])
                || part.contains(['<', '>', '"', '|', '?', '*'])
                || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || ["COM", "LPT"].iter().any(|prefix| {
                    stem.strip_prefix(prefix).is_some_and(|suffix| {
                        suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_digit()
                    })
                })
        })
    {
        return Err(FileError::new(
            403,
            "只允许共享目录内的普通相对路径，不允许隐藏文件或路径穿越",
        ));
    }
    Ok(())
}

fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

fn validate_markdown(path: &str) -> Result<()> {
    validate_relative(path, false)?;
    if !is_markdown(path) {
        return Err(FileError::new(403, "仅支持 .md 和 .markdown 文件"));
    }
    Ok(())
}

fn revision(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    path: String,
    content: String,
    // null is required when creating; an existing file always needs its revision.
    revision: Value,
}

pub struct FileStore {
    root: Dir,
    pub writable: bool,
    access: Mutex<()>,
}

impl FileStore {
    pub fn open(root: &str, writable: bool) -> io::Result<Self> {
        Ok(Self {
            root: Dir::open_ambient_dir(root, ambient_authority())?,
            writable,
            access: Mutex::new(()),
        })
    }

    // All subsequent filesystem calls are relative to a directory capability.
    // Unlike a canonicalize-then-open check, this also confines symlink races.
    fn directory(&self, path: &str) -> Result<Dir> {
        validate_relative(path, true)?;
        let mut current = self.root.try_clone()?;
        if !path.is_empty() {
            for part in path.split('/') {
                let metadata = current.symlink_metadata(part)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(FileError::new(403, "不允许访问符号链接或非普通目录"));
                }
                current = current.open_dir(part)?;
            }
        }
        Ok(current)
    }

    fn parent(&self, path: &str) -> Result<(Dir, String)> {
        validate_markdown(path)?;
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok((self.directory(parent)?, name.to_owned()))
    }

    fn read_at(parent: &Dir, name: &str) -> Result<String> {
        let metadata = parent.symlink_metadata(name)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(FileError::new(403, "不允许访问符号链接或非普通文件"));
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(FileError::new(413, "文件超过 512 KiB 限制"));
        }
        let file = parent.open(name)?;
        if !file.metadata()?.is_file() {
            return Err(FileError::new(403, "不是普通文件"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(FileError::new(413, "文件超过 512 KiB 限制"));
        }
        String::from_utf8(bytes).map_err(|_| FileError::new(415, "文件必须使用 UTF-8 编码"))
    }

    fn check_cancel(cancel: &CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            Err(FileError::new(499, "文件操作已取消"))
        } else {
            Ok(())
        }
    }

    pub fn handle(
        &self,
        request: FileRequest,
        body: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let _guard = self
            .access
            .lock()
            .map_err(|_| FileError::new(500, "文件服务不可用"))?;
        Self::check_cancel(cancel)?;
        match request {
            FileRequest::Config => {
                Ok(json!({"enabled":true, "writable":self.writable, "maxFileBytes":MAX_FILE_BYTES}))
            }
            FileRequest::List(path) => {
                let dir = self.directory(&path)?;
                let mut entries = Vec::new();
                for (index, entry) in dir.entries()?.enumerate() {
                    Self::check_cancel(cancel)?;
                    if index >= MAX_DIRECTORY_ENTRIES {
                        return Err(FileError::new(
                            413,
                            "目录项目超过 2000 个，请在 Agent 上共享更小的目录",
                        ));
                    }
                    let entry = entry?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if validate_relative(&name, false).is_err() {
                        continue;
                    }
                    let kind = entry.file_type()?;
                    if kind.is_symlink() || !(kind.is_dir() || kind.is_file() && is_markdown(&name))
                    {
                        continue;
                    }
                    let relative = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}/{name}")
                    };
                    if validate_relative(&relative, false).is_err() {
                        continue;
                    }
                    entries.push(json!({"name":name, "path":relative, "kind":if kind.is_dir() { "directory" } else { "file" }}));
                }
                entries.sort_by(|a, b| {
                    a["kind"]
                        .as_str()
                        .cmp(&b["kind"].as_str())
                        .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
                });
                Ok(json!({"path":path, "entries":entries}))
            }
            FileRequest::Read(path) => {
                let (parent, name) = self.parent(&path)?;
                let content = Self::read_at(&parent, &name)?;
                Ok(json!({"path":path, "revision":revision(&content), "content":content}))
            }
            FileRequest::Write => self.write(body, cancel),
        }
    }

    fn write(&self, body: Option<&str>, cancel: &CancellationToken) -> Result<Value> {
        if !self.writable {
            return Err(FileError::new(
                403,
                "共享目录为只读；请在 Agent 上设置 FILE_WRITE_ENABLED=true",
            ));
        }
        let request: WriteRequest = serde_json::from_str(body.unwrap_or_default())
            .map_err(|_| FileError::new(400, "保存需要 path、content 和 revision"))?;
        if request.content.len() > MAX_FILE_BYTES {
            return Err(FileError::new(413, "文件超过 512 KiB 限制"));
        }
        let (parent, name) = self.parent(&request.path)?;
        let expected = if request.revision.is_null() {
            None
        } else {
            Some(
                request
                    .revision
                    .as_str()
                    .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| FileError::new(400, "无效的文件版本"))?,
            )
        };
        let check_revision = || -> Result<()> {
            let actual = match Self::read_at(&parent, &name) {
                Ok(content) => Some(revision(&content)),
                Err(error) if error.status == 404 => None,
                Err(error) => return Err(error),
            };
            if actual.as_deref() != expected {
                return Err(FileError::new(
                    409,
                    "文件已被修改、创建或删除，请重新读取后再保存",
                ));
            }
            Ok(())
        };
        check_revision()?;
        let temporary = PathBuf::from(format!(".ai-remote-{}.tmp", Uuid::new_v4()));
        let mut file =
            parent.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
        let pending = PendingFile {
            parent: &parent,
            path: &temporary,
        };
        file.write_all(request.content.as_bytes())?;
        // Preserve the existing file's basic permissions when replacing it.
        if expected.is_some() {
            file.set_permissions(parent.symlink_metadata(&name)?.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        check_revision()?;
        Self::check_cancel(cancel)?;
        if expected.is_none() {
            // A hard link publishes a complete new file without overwriting an
            // existing destination, including one created by a local editor.
            parent
                .hard_link(&temporary, &parent, &name)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::AlreadyExists {
                        FileError::new(409, "文件已存在，请重新读取")
                    } else {
                        error.into()
                    }
                })?;
        } else {
            parent.rename(&temporary, &parent, &name)?;
        }
        drop(pending);
        Ok(json!({"path":request.path, "revision":revision(&request.content)}))
    }
}

struct PendingFile<'a> {
    parent: &'a Dir,
    path: &'a Path,
}
impl Drop for PendingFile<'_> {
    fn drop(&mut self) {
        let _ = self.parent.remove_file(self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::Arc};

    struct Fixture {
        path: PathBuf,
        store: Arc<FileStore>,
    }
    impl Fixture {
        fn new(writable: bool) -> Self {
            let path = std::env::temp_dir().join(format!("ai-remote-files-{}", Uuid::new_v4()));
            fs::create_dir_all(path.join("shared/笔记")).unwrap();
            fs::write(path.join("shared/笔记/欢迎.md"), "# 你好 🌍\n").unwrap();
            fs::write(path.join("shared/private.txt"), "not markdown").unwrap();
            fs::write(path.join("shared/.hidden.md"), "hidden").unwrap();
            fs::write(path.join("outside.md"), "outside").unwrap();
            let store =
                Arc::new(FileStore::open(path.join("shared").to_str().unwrap(), writable).unwrap());
            Self { path, store }
        }
        fn handle(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
            self.store.handle(
                parse_request(method, path)?,
                body.as_ref().map(Value::to_string).as_deref(),
                &CancellationToken::new(),
            )
        }
        fn write(&self, path: &str, content: &str, revision: Value) -> Result<Value> {
            self.handle(
                "PUT",
                "/files/write",
                Some(json!({"path":path, "content":content, "revision":revision})),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn browses_nested_markdown_and_preserves_unicode() {
        let fixture = Fixture::new(true);
        let list = fixture.handle("GET", "/files/list", None).unwrap();
        assert_eq!(
            list["entries"],
            json!([{"name":"笔记", "path":"笔记", "kind":"directory"}])
        );
        let content = fixture
            .handle(
                "GET",
                "/files/read?path=%E7%AC%94%E8%AE%B0%2F%E6%AC%A2%E8%BF%8E.md",
                None,
            )
            .unwrap();
        assert_eq!(content["content"], "# 你好 🌍\n");
        let saved = fixture
            .write(
                "笔记/欢迎.md",
                "# Updated\n中文",
                content["revision"].clone(),
            )
            .unwrap();
        assert_ne!(saved["revision"], content["revision"]);
        assert_eq!(
            fs::read_to_string(fixture.path.join("shared/笔记/欢迎.md")).unwrap(),
            "# Updated\n中文"
        );
    }

    #[test]
    fn creates_files_without_overwriting_existing_content() {
        let fixture = Fixture::new(true);
        fixture.write("new.markdown", "", Value::Null).unwrap();
        assert_eq!(
            fixture
                .write("new.markdown", "overwrite", Value::Null)
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("shared/new.markdown")).unwrap(),
            ""
        );
        assert!(fs::read_dir(fixture.path.join("shared"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".ai-remote-")));
    }

    #[test]
    fn rejects_read_only_writes_and_cancelled_writes() {
        let fixture = Fixture::new(false);
        assert_eq!(
            fixture
                .write("new.md", "hello", Value::Null)
                .unwrap_err()
                .status,
            403
        );
        assert!(fixture
            .handle("GET", "/files/read?path=笔记/欢迎.md", None)
            .is_ok());
        let fixture = Fixture::new(true);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            fixture
                .store
                .handle(
                    FileRequest::Write,
                    Some(r#"{"path":"new.md","content":"x","revision":null}"#),
                    &cancel
                )
                .unwrap_err()
                .status,
            499
        );
        assert!(!fixture.path.join("shared/new.md").exists());
    }

    #[test]
    fn rejects_traversal_hidden_paths_and_non_markdown() {
        let fixture = Fixture::new(true);
        for path in [
            "../outside.md",
            "/outside.md",
            "笔记/../../outside.md",
            "C:/outside.md",
            "C:\\outside.md",
            "a//b.md",
            ".hidden.md",
            "a/./b.md",
            "%2e%2e/outside.md",
            "private.txt",
            "a.md:stream",
            "NUL.md",
            "a\0.md",
        ] {
            let encoded: String = url::form_urlencoded::byte_serialize(path.as_bytes()).collect();
            assert_eq!(
                fixture
                    .handle("GET", &format!("/files/read?path={encoded}"), None)
                    .unwrap_err()
                    .status,
                403,
                "{path}"
            );
            assert_eq!(
                fixture
                    .write(path, "overwrite", Value::Null)
                    .unwrap_err()
                    .status,
                403,
                "{path}"
            );
        }
        assert_eq!(
            fs::read_to_string(fixture.path.join("outside.md")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn rejects_invalid_routes_methods_and_request_bodies() {
        let fixture = Fixture::new(true);
        for (method, path) in [
            ("DELETE", "/files/read?path=a.md"),
            ("POST", "/files/write"),
            ("GET", "/files/config/extra"),
            ("GET", "/files/read?path=a.md&path=b.md"),
            ("GET", "/files/list?unexpected=x"),
        ] {
            assert!(parse_request(method, path).is_err());
        }
        for body in [
            json!({}),
            json!({"path":"a.md", "content":"x"}),
            json!({"path":"a.md", "content":"x", "revision":false}),
        ] {
            assert_eq!(
                fixture
                    .handle("PUT", "/files/write", Some(body))
                    .unwrap_err()
                    .status,
                400
            );
        }
    }

    #[test]
    fn rejects_oversized_and_non_utf8_files() {
        let fixture = Fixture::new(true);
        fs::write(fixture.path.join("shared/binary.md"), [0xff, 0xfe]).unwrap();
        fs::write(
            fixture.path.join("shared/large.md"),
            vec![b'x'; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            fixture
                .handle("GET", "/files/read?path=binary.md", None)
                .unwrap_err()
                .status,
            415
        );
        assert_eq!(
            fixture
                .handle("GET", "/files/read?path=large.md", None)
                .unwrap_err()
                .status,
            413
        );
        assert_eq!(
            fixture
                .write("large.md", &"x".repeat(MAX_FILE_BYTES + 1), Value::Null)
                .unwrap_err()
                .status,
            413
        );
        assert_eq!(
            fixture
                .handle("GET", "/files/read?path=missing.md", None)
                .unwrap_err()
                .status,
            404
        );
    }

    #[test]
    fn rejects_changes_made_since_the_file_was_read() {
        let fixture = Fixture::new(true);
        let original = fixture
            .handle("GET", "/files/read?path=笔记/欢迎.md", None)
            .unwrap();
        fs::write(fixture.path.join("shared/笔记/欢迎.md"), "local edit").unwrap();
        assert_eq!(
            fixture
                .write("笔记/欢迎.md", "remote edit", original["revision"].clone())
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("shared/笔记/欢迎.md")).unwrap(),
            "local edit"
        );
    }

    #[test]
    fn serializes_concurrent_saves_against_the_same_revision() {
        let fixture = Fixture::new(true);
        let original = fixture
            .handle("GET", "/files/read?path=笔记/欢迎.md", None)
            .unwrap();
        let saves: Vec<_> = (0..2).map(|index| {
            let store = fixture.store.clone();
            let body = json!({"path":"笔记/欢迎.md", "content":format!("edit {index}"), "revision":original["revision"]}).to_string();
            std::thread::spawn(move || store.handle(FileRequest::Write, Some(&body), &CancellationToken::new()))
        }).collect();
        let mut statuses: Vec<_> = saves
            .into_iter()
            .map(|save| {
                save.join()
                    .unwrap()
                    .map(|_| 200)
                    .unwrap_or_else(|error| error.status)
            })
            .collect();
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_replaces_hardlinks_without_modifying_the_target() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(true);
        symlink(
            fixture.path.join("outside.md"),
            fixture.path.join("shared/link.md"),
        )
        .unwrap();
        symlink(&fixture.path, fixture.path.join("shared/escape")).unwrap();
        for path in ["link.md", "escape/outside.md"] {
            assert_eq!(
                fixture
                    .handle("GET", &format!("/files/read?path={path}"), None)
                    .unwrap_err()
                    .status,
                403
            );
            assert_eq!(
                fixture.write(path, "bad", Value::Null).unwrap_err().status,
                403
            );
        }
        let list = fixture.handle("GET", "/files/list", None).unwrap();
        assert!(!list.to_string().contains("link.md"));
        assert!(!list.to_string().contains("escape"));
        fs::hard_link(
            fixture.path.join("outside.md"),
            fixture.path.join("shared/hard.md"),
        )
        .unwrap();
        let original = fixture
            .handle("GET", "/files/read?path=hard.md", None)
            .unwrap();
        fixture
            .write("hard.md", "replacement", original["revision"].clone())
            .unwrap();
        assert_eq!(
            fs::read_to_string(fixture.path.join("outside.md")).unwrap(),
            "outside"
        );
    }
}
