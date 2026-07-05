//! Thunder download client.
//!
//! A thin, typed HTTP client that drives the closed-source 迅雷 CGI
//! (`xunlei-pan-cli-web`, exposed by the frontend server) through its
//! `drive/v1` REST API. thunder itself implements no download logic — this
//! module simply replays the browser's JSON calls against the local proxy.
//!
//! The API contract was recovered by statically reverse-engineering the
//! official spk (see the `xunlei-api-reverse` analysis). Endpoints proven
//! from the embedded web bundle are marked HIGH confidence; a few
//! (create/resolve request-body shapes) are inferred and best confirmed on a
//! live instance via `thunder dl raw`.
//!
//! Local calls to the whitelisted `drive/v1/*` paths do NOT require the
//! `pan_auth` token (the engine allows them via `driveApiAllowLocalToken`),
//! so this client sends plain JSON with no signing.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::time::Duration;

/// Web UI base prefix that every drive/v1 path hangs off of.
const API_PREFIX: &str = "/webman/3rdparty/pan-xunlei-com/index.cgi";

/// The storage space. Verified live: the local download space is the EMPTY
/// string (the default device space); `user#download` is only the task `type`
/// discriminator, not the space value.
const SPACE: &str = "";
const TASK_TYPE: &str = "user#download";

/// A resource (file) inside a resolved magnet/torrent/URL.
#[derive(Debug, Clone)]
pub struct ResolvedFile {
    pub index: i64,
    pub name: String,
    pub size: i64,
    pub is_dir: bool,
}

/// The outcome of resolving a link: an overall name plus the file list.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub list_id: String,
    pub name: String,
    pub total_size: i64,
    pub files: Vec<ResolvedFile>,
}

/// A download task as seen when listing.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub file_size: i64,
    /// 0-100.
    pub progress: i64,
    /// PHASE_TYPE_RUNNING / PENDING / PAUSED / ERROR / COMPLETE.
    pub phase: String,
    /// Bytes/sec, read from `params["download_speed"]`; 0 if absent.
    pub speed: i64,
    pub message: String,
}

impl Task {
    /// Human phase label, stripped of the `PHASE_TYPE_` prefix.
    pub fn phase_label(&self) -> &str {
        self.phase
            .strip_prefix("PHASE_TYPE_")
            .unwrap_or(&self.phase)
    }
}

/// A file entry in the cloud drive.
#[derive(Debug, Clone)]
pub struct CloudFile {
    pub id: String,
    pub name: String,
    pub size: i64,
    pub is_dir: bool,
}

pub struct ThunderClient {
    agent: ureq::Agent,
    /// e.g. `http://127.0.0.1:5055`
    base: String,
    /// Optional panel password → access_token cookie (only if the server was
    /// started with `--auth-password`). Localhost-no-auth leaves this None.
    cookie: Option<String>,
    /// The `pan-auth` JWT scraped from the Web UI homepage, cached lazily.
    /// Required by non-whitelisted endpoints (resource/list, singular task).
    pan_auth: RefCell<Option<String>>,
}

impl ThunderClient {
    pub fn new(host: &str, password: Option<&str>) -> Result<Self> {
        let base = normalize_host(host);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build();

        let mut client = Self {
            agent,
            base,
            cookie: None,
            pan_auth: RefCell::new(None),
        };

        if let Some(pw) = password {
            client.cookie = Some(client.login(pw)?);
        }
        Ok(client)
    }

    /// Fetch (and cache) the `pan-auth` token that the CGI injects into the
    /// Web UI homepage as `uiauth(value){ return "<jwt>" }`. The token is a
    /// 72h HS256 JWT; scraping it avoids having to reproduce the signing key.
    fn pan_auth_token(&self) -> Result<String> {
        if let Some(t) = self.pan_auth.borrow().as_ref() {
            return Ok(t.clone());
        }
        // The homepage lives at the CGI web-ui home path.
        let url = format!("{}{}", self.base, crate::constant::SYNOPKG_WEB_UI_HOME);
        let html = self
            .with_auth(self.agent.get(&url))
            .call()
            .context("fetch UI homepage for pan-auth token")?
            .into_string()
            .context("read UI homepage")?;

        let token = scrape_pan_auth(&html).ok_or_else(|| {
            anyhow!("could not find pan-auth token in UI homepage (is the engine logged in?)")
        })?;
        *self.pan_auth.borrow_mut() = Some(token.clone());
        Ok(token)
    }

    /// Exchange the panel password for an `access_token` cookie via `/login`.
    fn login(&self, password: &str) -> Result<String> {
        let url = format!("{}/login", self.base);
        // The login form does not follow redirects here; we just want the
        // Set-Cookie header off the 303 response.
        let resp = self
            .agent
            .post(&url)
            .send_form(&[("password", password)]);

        let resp = match resp {
            Ok(r) => r,
            // A 3xx is surfaced by ureq as an Err(Status) only when redirects
            // are disabled; treat any response we can read cookies from as ok.
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e).context("login request failed"),
        };

        let set_cookie = resp
            .header("set-cookie")
            .ok_or_else(|| anyhow!("login failed: no cookie returned (wrong password?)"))?;
        // Keep just the `access_token=...` pair.
        let token = set_cookie
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(anyhow!("login failed: empty cookie"));
        }
        Ok(token)
    }

    // ---- low-level transport --------------------------------------------

    /// Build a full URL for an API path (relative to the CGI prefix).
    fn url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("{}{}/{}", self.base, API_PREFIX, path)
    }

    /// Attach the auth cookie if we have one.
    fn with_auth(&self, req: ureq::Request) -> ureq::Request {
        match &self.cookie {
            Some(c) => req.set("Cookie", c),
            None => req,
        }
    }

    /// Issue a request against an API path and parse the JSON response.
    ///
    /// `path` is relative to the CGI prefix (e.g. `drive/v1/tasks`). `body`,
    /// when `Some`, is sent as a JSON request body. The `pan-auth` token is
    /// attached automatically (required by non-whitelisted endpoints, ignored
    /// by whitelisted ones).
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.request_inner(method, path, body, true)
    }

    /// Like [`request`] but without attaching the `pan-auth` token — used for
    /// bootstrap/connectivity endpoints (e.g. `device/now`) that are on the
    /// engine's local whitelist and must work before login.
    pub fn request_no_auth(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.request_inner(method, path, body, false)
    }

    fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        with_token: bool,
    ) -> Result<Value> {
        let url = self.url(path);
        let mut req = self.with_auth(self.agent.request(method, &url));
        if with_token {
            let token = self.pan_auth_token()?;
            req = req.set("pan-auth", &token);
        }

        let resp = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };

        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                return Err(map_api_error(code, &text));
            }
            Err(e) => return Err(e).context(format!("{method} {path} failed"))?,
        };

        let text = resp.into_string().context("read response body")?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .with_context(|| format!("{method} {path}: response is not JSON: {text}"))
    }

    // ---- high-level operations ------------------------------------------

    /// GET device/now → server unix time (seconds). Also the cheapest way to
    /// confirm connectivity + that the engine is up. Whitelisted (no token).
    pub fn device_now(&self) -> Result<i64> {
        let v = self.request_no_auth("GET", "device/now", None)?;
        v.get("now")
            .and_then(as_i64)
            .ok_or_else(|| anyhow!("device/now: missing `now` field: {v}"))
    }

    /// Resolve a magnet / http(s) URL into its file list.
    /// POST drive/v1/resource/list (requires the pan-auth token).
    ///
    /// The response is a recursive tree: the top-level resource is a directory
    /// whose `dir.resources` hold children, which may themselves be dirs
    /// (torrents carry junk subfolders). We flatten to leaf files, preserving
    /// each file's flat global `file_index` (the value `sub_file_index` wants).
    pub fn resolve(&self, url: &str) -> Result<Resolved> {
        let body = json!({
            "urls": url,
            "page_size": 2000,
            "thumbnail_type": "",
            "max_file_count": 2000,
        });
        let v = self.request("POST", "drive/v1/resource/list", Some(&body))?;

        let list_id = v
            .get("list_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let list = v.get("list").unwrap_or(&Value::Null);

        // Name: the top-level resource's name is the torrent/task title.
        let top = list
            .get("resources")
            .and_then(|x| x.as_array())
            .and_then(|a| a.first());
        let name = top
            .and_then(|r| r.get("name"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        // Recursively collect leaf (non-dir) files.
        let mut files = Vec::new();
        if let Some(resources) = list.get("resources").and_then(|x| x.as_array()) {
            collect_leaves(resources, &mut files);
        }
        // If nothing nested (a single plain URL), fall back to the top resource.
        if files.is_empty() {
            if let Some(r) = top {
                files.push(resource_to_file(r));
            }
        }

        let total_size = files.iter().map(|f| f.size).sum::<i64>();
        Ok(Resolved {
            list_id,
            name,
            total_size,
            files,
        })
    }

    /// Create a download task. POST drive/v1/task.
    ///
    /// `resolved` is the prior [`resolve`] result. `indices` selects which
    /// sub-files to download (empty = all). `name` overrides the task/folder
    /// name; `parent_folder_id` targets a cloud folder ("" = default root).
    pub fn add(
        &self,
        url: &str,
        resolved: &Resolved,
        name: Option<&str>,
        parent_folder_id: &str,
        indices: &[i64],
    ) -> Result<String> {
        let sub_file_index = if indices.is_empty() {
            // Convention observed in engine strings: "-1"/empty selects all.
            String::new()
        } else {
            indices
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };

        // file_size (top-level) must be a valid int64 string: the total bytes
        // of the selected files (or all files when nothing is selected).
        let selected_size: i64 = if indices.is_empty() {
            resolved.total_size
        } else {
            resolved
                .files
                .iter()
                .filter(|f| indices.contains(&f.index))
                .map(|f| f.size)
                .sum()
        };
        let task_name = name
            .filter(|s| !s.is_empty())
            .unwrap_or(resolved.name.as_str());

        // All `params` values are strings, even numeric ones (proto map<string,string>).
        let body = json!({
            "type": TASK_TYPE,
            "name": task_name,
            "file_size": selected_size.to_string(),
            "space": SPACE,
            "params": {
                "url": url,
                "parent_folder_id": parent_folder_id,
                "total_file_count": resolved.files.len().to_string(),
                "sub_file_index": sub_file_index,
                "mime_type": "",
            }
        });

        let v = self.request("POST", "drive/v1/task", Some(&body))?;
        let id = v
            .get("task")
            .and_then(|t| t.get("id"))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(id)
    }

    /// List entries inside a cloud folder (parent_id empty = drive root).
    /// GET drive/v1/files?parent_id=..&space=
    pub fn cloud_files(&self, parent_id: &str, limit: u32) -> Result<Vec<CloudFile>> {
        let path = format!(
            "drive/v1/files?space={}&parent_id={}&limit={}",
            urlencoding::encode(SPACE),
            urlencoding::encode(parent_id),
            limit
        );
        let v = self.request("GET", &path, None)?;
        let files = v
            .get("files")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(files.iter().map(json_to_cloud_file).collect())
    }

    /// Get the HTTPS direct-download link for a cloud file.
    /// GET drive/v1/files/{id} → `web_content_link`.
    pub fn cloud_file_link(&self, file_id: &str) -> Result<String> {
        let path = format!("drive/v1/files/{}?space={}", file_id, urlencoding::encode(SPACE));
        let v = self.request("GET", &path, None)?;
        let link = v
            .get("web_content_link")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if link.is_empty() {
            return Err(anyhow!(
                "no web_content_link for file {file_id} (not ready, or access restricted)"
            ));
        }
        Ok(link)
    }

    /// Recursively collect all leaf files under a cloud folder (or the file
    /// itself if `file_id` is a file). Returns (relative_path, CloudFile).
    pub fn cloud_walk(&self, file_id: &str, name: &str) -> Result<Vec<(String, CloudFile)>> {
        let mut out = Vec::new();
        // Probe: list children; if empty and it's addressable as a file, treat
        // as a single file.
        let children = self.cloud_files(file_id, 200).unwrap_or_default();
        if children.is_empty() {
            // A single file. Fall back to the id as the filename when the
            // caller didn't supply a name (e.g. `dl pull <file_id>`).
            let fname = if name.is_empty() { file_id } else { name };
            out.push((fname.to_string(), CloudFile {
                id: file_id.to_string(),
                name: fname.to_string(),
                size: 0,
                is_dir: false,
            }));
            return Ok(out);
        }
        for c in children {
            let rel = if name.is_empty() {
                c.name.clone()
            } else {
                format!("{}/{}", name, c.name)
            };
            if c.is_dir {
                out.extend(self.cloud_walk(&c.id, &rel)?);
            } else {
                out.push((rel, c));
            }
        }
        Ok(out)
    }

    /// Download a cloud file's bytes to a local path via its direct link.
    /// Streams the HTTPS response straight to disk (pure HTTP, no P2P).
    pub fn download_to(&self, file_id: &str, dest: &std::path::Path) -> Result<u64> {
        let link = self.cloud_file_link(file_id)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let resp = self
            .agent
            .get(&link)
            .call()
            .with_context(|| format!("GET {link} failed"))?;
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(dest)
            .with_context(|| format!("create {}", dest.display()))?;
        let n = std::io::copy(&mut reader, &mut file).context("stream download to disk")?;
        Ok(n)
    }

    /// List tasks. GET drive/v1/tasks?space=..&type=..&limit=..
    /// `only_active` narrows to pending+running via the `filters` param.
    pub fn list(&self, only_active: bool, limit: u32) -> Result<Vec<Task>> {
        let mut path = format!(
            "drive/v1/tasks?space={}&type={}&limit={}",
            urlencoding::encode(SPACE),
            urlencoding::encode(TASK_TYPE),
            limit
        );
        if only_active {
            let filters = r#"{"phase":{"in":"PHASE_TYPE_PENDING,PHASE_TYPE_RUNNING"}}"#;
            path.push_str(&format!("&filters={}", urlencoding::encode(filters)));
        }

        let v = self.request("GET", &path, None)?;
        let tasks = v
            .get("tasks")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tasks.iter().map(json_to_task).collect())
    }

    /// Pause a task. PATCH drive/v1/task with set_params.spec = {"phase":"pause"}.
    pub fn pause(&self, id: &str) -> Result<()> {
        self.operate(id, "pause")
    }

    /// Resume a task. PATCH drive/v1/task with set_params.spec = {"phase":"running"}.
    pub fn resume(&self, id: &str) -> Result<()> {
        self.operate(id, "running")
    }

    fn operate(&self, id: &str, phase_action: &str) -> Result<()> {
        // Note the double JSON encoding: `spec` is a JSON *string*.
        let spec = json!({ "phase": phase_action }).to_string();
        let body = json!({
            "id": id,
            "space": SPACE,
            "type": TASK_TYPE,
            "set_params": { "spec": spec }
        });
        self.request("PATCH", "drive/v1/task", Some(&body))?;
        Ok(())
    }

    /// Delete tasks. DELETE drive/v1/tasks?space=..&task_ids=a&task_ids=b.
    /// `delete_files` also removes downloaded data (schema-supported; may be
    /// ignored by the engine on this route).
    pub fn remove(&self, ids: &[String], delete_files: bool) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut path = format!("drive/v1/tasks?space={}", urlencoding::encode(SPACE));
        for id in ids {
            path.push_str(&format!("&task_ids={}", urlencoding::encode(id)));
        }
        if delete_files {
            path.push_str("&delete_files=true");
        }
        self.request("DELETE", &path, None)?;
        Ok(())
    }
}

// ---- helpers ------------------------------------------------------------

fn normalize_host(host: &str) -> String {
    let h = host.trim().trim_end_matches('/');
    if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else {
        format!("http://{h}")
    }
}

/// Numeric fields arrive either as JSON numbers or as quoted strings
/// (proto int64 → string). Accept both.
fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

fn resource_to_file(r: &Value) -> ResolvedFile {
    ResolvedFile {
        index: r.get("file_index").and_then(as_i64).unwrap_or(0),
        name: r.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        size: r.get("file_size").and_then(as_i64).unwrap_or(0),
        is_dir: r.get("is_dir").and_then(|x| x.as_bool()).unwrap_or(false),
    }
}

/// Recursively walk a resource tree, appending leaf (non-dir) files. The
/// `file_index` on each leaf is a flat global index across the whole tree.
fn collect_leaves(resources: &[Value], out: &mut Vec<ResolvedFile>) {
    for r in resources {
        let is_dir = r.get("is_dir").and_then(|x| x.as_bool()).unwrap_or(false);
        if is_dir {
            if let Some(children) = r.get("dir").and_then(|d| d.get("resources")).and_then(|x| x.as_array()) {
                collect_leaves(children, out);
            }
        } else {
            out.push(resource_to_file(r));
        }
    }
}

/// Extract the `pan-auth` JWT the CGI injects into the homepage as
/// `uiauth(value){ return "<jwt>" }`. Matches the first `eyJ...` token.
fn scrape_pan_auth(html: &str) -> Option<String> {
    let start = html.find("eyJ")?;
    let rest = &html[start..];
    // A JWT is base64url segments joined by '.', so accept those chars.
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    let token = &rest[..end];
    // Sanity: a JWT has exactly two dots (three segments).
    if token.matches('.').count() == 2 && token.len() > 20 {
        Some(token.to_string())
    } else {
        None
    }
}

fn json_to_task(v: &Value) -> Task {
    let speed = v
        .get("params")
        .and_then(|p| p.get("download_speed"))
        .and_then(as_i64)
        .unwrap_or(0);
    Task {
        id: v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        file_size: v.get("file_size").and_then(as_i64).unwrap_or(0),
        progress: v.get("progress").and_then(as_i64).unwrap_or(0),
        phase: v.get("phase").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        speed,
        message: v.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    }
}

fn json_to_cloud_file(v: &Value) -> CloudFile {
    // Cloud files use kind "drive#folder" / "drive#file".
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
    CloudFile {
        id: v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        size: v.get("size").and_then(as_i64).unwrap_or(0),
        is_dir: kind.ends_with("folder"),
    }
}
fn map_api_error(code: u16, body: &str) -> anyhow::Error {
    let hint = if body.contains("SPACE_FOLDER_NOT_EXIST") || body.contains("WRONG_SPACE_TO_GET_FILE") {
        "\n  hint: the download space/folder is invalid — is the engine logged in and running?"
    } else if body.contains("COPYRIGHT") {
        "\n  hint: this resource is blocked for copyright reasons (版权拦截)."
    } else if body.contains("PARSE_TORRENT_FAILED") || body.contains("TORRENT_FAILED") {
        "\n  hint: failed to parse the torrent/magnet — the link may be dead or malformed."
    } else if body.contains("VIP") || body.contains("vip") {
        "\n  hint: this may require a 迅雷 VIP account, or you hit the non-VIP daily task limit."
    } else if code == 401 || code == 403 {
        "\n  hint: authentication rejected — pass --password if the panel has one set."
    } else {
        ""
    };
    anyhow!("API error {code}: {body}{hint}")
}
