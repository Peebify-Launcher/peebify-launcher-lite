// ------------ Neverness to Everness ------------
// Neverness to Everness publishes an encrypted resource list, and each file is fetched from a content-addressed URL
// (by md5 and size), whole or in blocks written into the file in place. This reads that list and the launcher runtime
// feed into a manifest for the install, repair and verify engines, and answers the install preview and the voice pack
// list.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::engine::{merge_parts, offline_or, JobKind, Preview};
use super::fs_util::md5_hex;
use super::manifest::{Cx, Entry, Hash, Manifest, Policy, Publisher, Segment, Source};
use super::validator::{Record, Triage};
use super::{err_response, game_profiles, http, members, ok_with, BackendState};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const CONTAINER_MAGIC: &[u8] = b"PatcherXML0\0";
const CONTAINER_HEADER_LEN: usize = 16;
const CRYPTO_IV: &[u8; 16] = b"PatcherSDK000000";
const BASE_TAG: &str = "baseTag";
const LAUNCHER_DIR: &str = "NTEGlobal";
const MAX_LIST_BYTES: u64 = 64 * 1024 * 1024;

const MAX_BRANCH_HOPS: usize = 3;

pub struct Config {
    pub res_version: String,
    pub list_hash: String,
    pub bases: Vec<String>,
}

pub fn tag_language(tag: &str) -> Option<&'static str> {
    match tag {
        "pakchunk101" => Some("Chinese"),
        "pakchunk102" => Some("English"),
        "pakchunk103" => Some("Japanese"),
        "pakchunk104" => Some("Korean"),
        _ => None,
    }
}

fn res_roots(profile: &Value) -> Vec<String> {
    profile
        .get("nteResUrls")
        .and_then(Value::as_array)
        .map(|urls| {
            urls.iter()
                .filter_map(|u| u.as_str())
                .filter(|u| !u.is_empty())
                .map(|u| u.trim_end_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn bases_of(roots: &[String], branch: &str) -> Vec<String> {
    roots.iter().map(|root| format!("{root}/{branch}")).collect()
}

fn branch(profile: &Value) -> &str {
    profile
        .get("nteBranch")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("publish_PC")
}

fn app_id(profile: &Value) -> &str {
    profile
        .get("nteAppId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("3000001")
}

fn crypto_key(profile: &Value) -> [u8; 16] {
    let mut key = [b'0'; 16];
    let source = format!("{}@Patcher0", app_id(profile));
    for (slot, byte) in key.iter_mut().zip(source.bytes()) {
        *slot = byte;
    }
    key
}

fn object_url(base: &str, md5: &str, size: u64) -> String {
    let shard = md5.chars().next().unwrap_or('0');
    format!("{base}/Res/{shard}/{md5}.{size}")
}

fn xml_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

fn attribute<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = raw;
    loop {
        let at = rest.find(name)?;
        let after = &rest[at + name.len()..];
        let before_ok = at == 0
            || rest[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let trimmed = after.trim_start();
        if before_ok && trimmed.starts_with('=') {
            let quoted = trimmed[1..].trim_start();
            let quote = quoted.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = quoted[1..].find(quote)? + 1;
                return Some(&quoted[1..end]);
            }
        }
        rest = &rest[at + name.len()..];
    }
}

fn need<'a>(body: &'a str, name: &str, owner: &str) -> Result<&'a str, String> {
    attribute(body, name).ok_or_else(|| format!("{owner} without {name}"))
}

fn need_number(body: &str, name: &str, owner: &str) -> Result<u64, String> {
    need(body, name, owner)?.trim().parse().map_err(|_| format!("{owner} with a bad {name}"))
}

fn md5_bytes(text: &str, owner: &str) -> Result<[u8; 16], String> {
    let mut md5 = [0u8; 16];
    hex::decode_to_slice(text.trim(), &mut md5).map_err(|_| format!("{owner} with a bad md5"))?;
    Ok(md5)
}

#[derive(Debug, PartialEq)]
struct Redirect {
    branch: String,
    roots: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Feed {
    res_version: Option<String>,
    list_hash: Option<String>,
    redirect: Option<Redirect>,
}

pub async fn fetch_config(profile: &Value) -> Result<Config, String> {
    let mut roots = res_roots(profile);
    if roots.is_empty() {
        return Err("no nteResUrls for profile".to_string());
    }
    let mut branch = branch(profile).to_string();
    let mut visited = vec![branch.clone()];
    for _ in 0..=MAX_BRANCH_HOPS {
        let bases = bases_of(&roots, &branch);
        let feed = read_feed(&bases).await?;
        match feed.redirect.filter(|r| !visited.contains(&r.branch)) {
            Some(next) => {
                log::info!("nte: the {branch} feed points at branch {}", next.branch);
                branch = next.branch;
                visited.push(branch.clone());
                if !next.roots.is_empty() {
                    roots = next.roots;
                }
            }
            None => {
                return Ok(Config {
                    res_version: feed.res_version.ok_or("NTE config.xml has no ResVersion")?,
                    list_hash: feed.list_hash.ok_or("NTE config.xml has no listHash")?,
                    bases,
                })
            }
        }
    }
    Err("The NTE version feed points from branch to branch without end".to_string())
}

async fn read_feed(bases: &[String]) -> Result<Feed, String> {
    let mut last = String::new();
    for base in bases {
        let url = format!("{base}/Version/Windows/config.xml");
        match http::get_text(&url).await {
            Ok(xml) => return Ok(parse_config(&xml)),
            Err(e) => {
                log::warn!("nte: version feed {url} failed: {e}");
                last = e;
            }
        }
    }
    Err(format!("Could not read the NTE version feed: {last}"))
}

fn parse_config(xml: &str) -> Feed {
    Feed {
        res_version: xml_value(xml, "ResVersion").filter(|v| !v.is_empty()).map(str::to_string),
        list_hash: xml_value(xml, "listHash").filter(|v| v.len() == 32).map(str::to_lowercase),
        redirect: xml_value(xml, "ResConfig").and_then(parse_redirect),
    }
}

fn parse_redirect(encoded: &str) -> Option<Redirect> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let config: Value = serde_json::from_slice(&raw).ok()?;
    let branch = config["branchName"]
        .as_str()
        .filter(|b| !b.is_empty() && b.len() <= 64 && b.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'))?
        .to_string();
    let roots = config["gameResUrl"]
        .as_array()
        .map(|urls| {
            urls.iter()
                .filter_map(Value::as_str)
                .filter(|u| u.starts_with("https://"))
                .map(|u| u.trim_end_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default();
    Some(Redirect { branch, roots })
}

// ------------ Resource List ------------
// Downloads and decrypts the resource list (an AES-wrapped container inside a zip) and parses it into file entries
// tagged with their content group. The last parsed list is cached by its hash.
pub fn decode_container(raw: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if raw.len() < CONTAINER_HEADER_LEN || !raw.starts_with(CONTAINER_MAGIC) {
        return Err("NTE resource list is not a PatcherXML container".to_string());
    }
    let declared = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;

    let mut buffer = raw[CONTAINER_HEADER_LEN..].to_vec();
    if buffer.is_empty() || !buffer.len().is_multiple_of(16) {
        return Err("NTE resource list payload is not block aligned".to_string());
    }
    let plain = Aes128CbcDec::new(key.into(), CRYPTO_IV.into())
        .decrypt_padded::<Pkcs7>(&mut buffer)
        .map_err(|_| "NTE resource list failed to decrypt".to_string())?;

    let mut xml = Vec::new();
    flate2::read::ZlibDecoder::new(plain)
        .take(MAX_LIST_BYTES)
        .read_to_end(&mut xml)
        .map_err(|e| format!("NTE resource list is not valid zlib: {e}"))?;

    if xml.len() != declared {
        return Err(format!(
            "NTE resource list expanded to {} bytes, expected {declared}",
            xml.len()
        ));
    }
    Ok(xml)
}

pub struct ResList {
    pub items: Vec<(String, Entry)>,
}

impl ResList {
    fn tag_totals(&self) -> Vec<(String, u64, u64)> {
        let mut totals: Vec<(String, u64, u64)> = Vec::new();
        for (tag, entry) in &self.items {
            match totals.iter_mut().find(|t| &t.0 == tag) {
                Some(total) => {
                    total.1 += entry.size;
                    total.2 += 1;
                }
                None => totals.push((tag.clone(), entry.size, 1)),
            }
        }
        totals
    }
}

static RESLIST_CACHE: Mutex<Option<(String, Arc<ResList>)>> = Mutex::new(None);
static RESLIST_FETCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cached_reslist(key: &str) -> Option<Arc<ResList>> {
    RESLIST_CACHE
        .lock()
        .as_ref()
        .filter(|(cached, _)| cached == key)
        .map(|(_, list)| Arc::clone(list))
}

pub async fn fetch_reslist(profile: &Value, config: &Config) -> Result<Arc<ResList>, String> {
    let bases = &config.bases;
    let first = bases.first().ok_or("no nteResUrls for profile")?;
    let key = format!("{}@{first}", config.list_hash);
    if let Some(list) = cached_reslist(&key) {
        return Ok(list);
    }
    let _flight = RESLIST_FETCH.lock().await;
    if let Some(list) = cached_reslist(&key) {
        return Ok(list);
    }
    let list = Arc::new(download_reslist(profile, config, bases).await?);
    *RESLIST_CACHE.lock() = Some((key, Arc::clone(&list)));
    Ok(list)
}

async fn download_reslist(profile: &Value, config: &Config, bases: &[String]) -> Result<ResList, String> {
    let key = crypto_key(profile);
    let mut last = String::new();

    for base in bases {
        let url = format!(
            "{base}/Version/Windows/version/{}/ResList.bin.zip",
            config.res_version
        );
        let xml = match fetch_reslist_xml(&url, &key, config).await {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("nte: resource list {url} failed: {e}");
                last = e;
                continue;
            }
        };
        let text = String::from_utf8(xml)
            .map_err(|e| format!("NTE resource list is not valid UTF-8: {e}"))?;
        return parse_reslist(&text, &bases[0]);
    }

    Err(format!("Could not read the NTE resource list: {last}"))
}

async fn fetch_reslist_xml(url: &str, key: &[u8; 16], config: &Config) -> Result<Vec<u8>, String> {
    let archive = http::get_bytes(url).await?;
    let container = members::read_member(archive.to_vec(), "ResList.bin", MAX_LIST_BYTES)?;
    let xml = decode_container(&container, key)?;
    let got = md5_hex(&xml);
    if got != config.list_hash {
        return Err(format!(
            "NTE resource list digest {got} does not match the {} advertised by config.xml",
            config.list_hash
        ));
    }
    Ok(xml)
}

struct Open {
    tag: String,
    path: String,
    size: u64,
    md5: String,
    blocks: Vec<Segment>,
}

impl Open {
    fn finish(mut self, base: &str) -> Result<(String, Entry), String> {
        let md5 = md5_bytes(&self.md5, "NTE resource list has a Res")?;
        let object = object_url(base, &self.md5, self.size).into_boxed_str();
        self.blocks.sort_by_key(|b| b.dst);
        let mut end = 0;
        let covers = self.blocks.iter().all(|b| {
            let contiguous = b.dst == end;
            end += b.size;
            contiguous
        }) && end == self.size;
        let source = match (self.size, covers) {
            (0, _) => Source::Segments { object, parts: Arc::from([]) },
            (_, true) => Source::Segments { object, parts: self.blocks.into() },
            (_, false) => Source::Url(object),
        };
        let entry = Entry { path: self.path.into(), size: self.size, hash: Hash::Md5(md5), source, live: true };
        Ok((self.tag, entry))
    }
}

pub fn parse_reslist(xml: &str, base: &str) -> Result<ResList, String> {
    let mut tag_stack: Vec<String> = vec![BASE_TAG.to_string()];
    let mut items: Vec<(String, Entry)> = Vec::new();
    let mut open: Option<Open> = None;
    let mut pak: Option<(String, u64)> = None;

    for (name, body, self_closing, closing) in tags(xml) {
        match (name, closing) {
            ("BaseVersion", false) => {
                let tag = attribute(body, "tag")
                    .map(str::to_string)
                    .unwrap_or_else(|| current_tag(&tag_stack));
                tag_stack.push(tag);
            }
            ("BaseVersion", true) if tag_stack.len() > 1 => {
                tag_stack.pop();
            }
            ("Res", false) => {
                let owner = "NTE resource list has a Res";
                let res = Open {
                    tag: current_tag(&tag_stack),
                    path: normalise(need(body, "filename", owner)?),
                    size: need_number(body, "filesize", owner)?,
                    md5: need(body, "md5", owner)?.to_lowercase(),
                    blocks: Vec::new(),
                };
                if self_closing {
                    items.push(res.finish(base)?);
                } else {
                    open = Some(res);
                }
            }
            ("Block", false) => {
                if let Some(res) = open.as_mut() {
                    let owner = "NTE resource list has a Block";
                    let start = need_number(body, "start", owner)?;
                    res.blocks.push(Segment {
                        src: start,
                        dst: start,
                        size: need_number(body, "size", owner)?,
                        md5: md5_bytes(need(body, "md5", owner)?, owner)?,
                    });
                }
            }
            ("Res", true) => {
                if let Some(res) = open.take() {
                    items.push(res.finish(base)?);
                }
            }
            ("Pak", false) => {
                let owner = "NTE resource list has a Pak";
                pak = Some((need(body, "md5", owner)?.to_lowercase(), need_number(body, "filesize", owner)?));
            }
            ("Pak", true) => pak = None,
            ("Entry", false) => {
                let owner = "NTE resource list has an Entry";
                let (pak_md5, pak_size) = pak.clone().ok_or("NTE resource list has an Entry outside a Pak")?;
                let size = need_number(body, "size", owner)?;
                let md5 = md5_bytes(need(body, "md5", owner)?, owner)?;
                let part = Segment { src: need_number(body, "offset", owner)?, dst: 0, size, md5 };
                let entry = Entry {
                    path: normalise(need(body, "name", owner)?).into(),
                    size,
                    hash: Hash::Md5(md5),
                    source: Source::Segments { object: object_url(base, &pak_md5, pak_size).into(), parts: Arc::from([part]) },
                    live: true,
                };
                items.push((current_tag(&tag_stack), entry));
            }
            _ => {}
        }
    }

    if items.is_empty() {
        return Err("NTE resource list contained no files".to_string());
    }
    Ok(ResList { items })
}

fn current_tag(stack: &[String]) -> String {
    stack
        .last()
        .cloned()
        .unwrap_or_else(|| BASE_TAG.to_string())
}

fn normalise(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_string()
}

fn tags(xml: &str) -> Vec<(&str, &str, bool, bool)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(open) = xml[at..].find('<') {
        let start = at + open + 1;
        let Some(close) = xml[start..].find('>') else {
            break;
        };
        let end = start + close;
        let raw = &xml[start..end];
        at = end + 1;
        if raw.starts_with('?') || raw.starts_with('!') {
            continue;
        }
        let closing = raw.starts_with('/');
        let self_closing = raw.ends_with('/');
        let inner = raw.trim_start_matches('/').trim_end_matches('/');
        let name_end = inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner.len());
        let name = &inner[..name_end];
        if name.is_empty() {
            continue;
        }
        out.push((name, &inner[name_end..], self_closing, closing));
        if self_closing && !closing {
            out.push((name, "", false, true));
        }
    }
    out
}

// ------------ Launcher Runtime Files ------------
// The game also needs a few launcher runtime files published on a separate feed (Version.ini plus a manifest). Each is
// a zip of one file, and they are installed with the game.
fn launcher_bases(profile: &Value) -> Vec<String> {
    profile
        .get("nteLauncherUrls")
        .and_then(Value::as_array)
        .map(|urls| {
            urls.iter()
                .filter_map(|u| u.as_str())
                .filter(|u| !u.is_empty())
                .map(|u| u.trim_end_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_launcher_manifest(xml: &str) -> Result<Vec<Entry>, String> {
    let mut base = String::new();
    let mut version = String::new();
    let mut files = Vec::new();

    for (name, body, _, closing) in tags(xml) {
        if closing {
            continue;
        }
        match name {
            "Url" => {
                if let Some(v) = attribute(body, "BaseUrl") {
                    base = v.trim_end_matches('/').to_string();
                }
            }
            "ProductVersion" => {
                if let Some(v) = attribute(body, "Version") {
                    version = v.to_string();
                }
            }
            "File" => {
                let owner = "NTE launcher manifest has a File";
                files.push((
                    normalise(need(body, "Path", owner)?),
                    need_number(body, "Size", owner)?,
                    md5_bytes(need(body, "Checksum", owner)?, owner)?,
                    need_number(body, "ZipSize", owner)?,
                    md5_bytes(need(body, "ZipChecksum", owner)?, owner)?,
                ));
            }
            _ => {}
        }
    }

    if base.is_empty() || version.is_empty() {
        return Err("NTE launcher manifest has no BaseUrl or ProductVersion".to_string());
    }
    if files.is_empty() {
        return Err("NTE launcher manifest listed no files".to_string());
    }
    Ok(files
        .into_iter()
        .map(|(path, size, md5, zip_size, zip_md5)| Entry {
            path: format!("{LAUNCHER_DIR}/{path}").into(),
            size,
            hash: if size == 0 { Hash::None } else { Hash::Md5(md5) },
            source: Source::Zip { url: format!("{base}/{version}/{path}.zip").into(), size: zip_size, hash: Hash::Md5(zip_md5) },
            live: true,
        })
        .collect())
}

pub async fn fetch_launcher(profile: &Value) -> Result<Vec<Entry>, String> {
    let bases = launcher_bases(profile);
    if bases.is_empty() {
        return Err("no nteLauncherUrls for profile".to_string());
    }

    let mut last = String::new();
    for base in &bases {
        let ini_url = format!("{base}/Version.ini");
        let ini = match http::get_text(&ini_url).await {
            Ok(text) => text,
            Err(e) => {
                log::warn!("nte: launcher feed {ini_url} failed: {e}");
                last = e;
                continue;
            }
        };
        let list_url = super::fs_util::ini_entries(&ini)
            .find(|(_, key, _)| key.eq_ignore_ascii_case("FileListURL"))
            .map(|(_, _, value)| value.to_string())
            .filter(|url| !url.is_empty());
        let Some(list_url) = list_url else {
            log::warn!("nte: launcher feed {ini_url} has no FileListURL");
            last = "NTE launcher Version.ini has no FileListURL".to_string();
            continue;
        };
        match http::get_text(&list_url).await {
            Ok(xml) => return parse_launcher_manifest(&xml),
            Err(e) => {
                log::warn!("nte: launcher manifest {list_url} failed: {e}");
                last = e;
            }
        }
    }
    Err(format!("Could not read the NTE launcher manifest: {last}"))
}

// ------------ Manifest ------------
// The game's files for the content groups in use: the base game and the voice packs picked (`None` picks them all),
// plus the launcher runtime. An install uses the packs picked now; repair and verify check the groups the install holds.
fn keeps(picked: Option<&[String]>, tag: &str) -> bool {
    tag == BASE_TAG || picked.is_none_or(|list| list.iter().any(|t| t == tag))
}

fn manifest_of(version: String, mirrors: Vec<String>, list: &ResList, launcher: Vec<Entry>, picked: Option<&[String]>) -> Manifest {
    let mut manifest = Manifest { version, mirrors, ..Manifest::default() };
    for (tag, entry) in list.items.iter().filter(|(tag, _)| keeps(picked, tag)) {
        if !manifest.groups.contains(tag) {
            manifest.groups.push(tag.clone());
        }
        manifest.entries.push(entry.clone());
    }
    manifest.entries.extend(launcher);
    manifest
}

async fn installed_groups(dir: &Path) -> Option<Vec<String>> {
    let dir = dir.to_path_buf();
    let groups = tauri::async_runtime::spawn_blocking(move || Record::load(&dir).groups).await.ok()?;
    (!groups.is_empty()).then_some(groups)
}

pub struct Nte;

impl Publisher for Nte {
    fn manifest<'a>(&'a self, cx: &'a Cx<'a>) -> BoxFuture<'a, Result<Manifest, String>> {
        Box::pin(async move {
            let config = fetch_config(cx.profile).await?;
            let (list, launcher) = tokio::join!(fetch_reslist(cx.profile, &config), fetch_launcher(cx.profile));
            let (list, launcher) = (list?, launcher?);
            let selected = game_profiles::content_tags(game_profiles::profile_id(cx.profile));
            let picked = match cx.purpose {
                JobKind::Install => selected,
                JobKind::Repair | JobKind::Verify => installed_groups(cx.dir).await.or(selected),
            };
            Ok(manifest_of(config.res_version, config.bases, &list, launcher, picked.as_deref()))
        })
    }

    fn triage(&self, dir: &Path, files: &[Entry]) -> Triage {
        Triage { trusted: Record::load(dir).triage(dir, files).trusted, ..Triage::default() }
    }

    fn latest<'a>(&'a self, profile: &'a Value, _fresh: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({ "version": fetch_config(profile).await?.res_version })) })
    }

    fn preview<'a>(&'a self, profile: &'a Value, _quality: Option<&'a str>) -> BoxFuture<'a, Result<Preview, String>> {
        Box::pin(preview(profile))
    }

    fn policy(&self, _dir: &Path) -> Policy {
        Policy { latest: true, records: true, ..Policy::default() }
    }
}

// ------------ Install Preview ------------
// The download and install size of the selected voice packs, and the packs on offer.
pub(super) async fn preview(profile: &Value) -> Result<Preview, String> {
    let (config_list, launcher) = tokio::join!(
        async {
            let config = fetch_config(profile).await?;
            let list = fetch_reslist(profile, &config).await?;
            Ok::<_, String>((config, list))
        },
        fetch_launcher(profile),
    );
    let (config, list) = config_list?;
    let launcher = launcher?;

    let wanted = game_profiles::content_tags(game_profiles::profile_id(profile));
    let mut entries: Vec<(String, String, u64, u64)> = list
        .tag_totals()
        .into_iter()
        .filter(|(tag, _, _)| keeps(wanted.as_deref(), tag))
        .map(|(tag, bytes, files)| {
            let label = if tag == BASE_TAG {
                "game files".to_string()
            } else {
                match tag_language(&tag) {
                    Some(language) => format!("{language} voice"),
                    None => "voice pack".to_string(),
                }
            };
            (tag, label, bytes, files)
        })
        .collect();
    let packed: u64 = launcher.iter().map(Entry::fetch_size).sum();
    entries.push(("launcher".to_string(), "launcher runtime".to_string(), packed, launcher.len() as u64));

    let chosen = list.items.iter().filter(|(tag, _)| keeps(wanted.as_deref(), tag));
    let game_bytes: u64 = chosen.clone().map(|(_, e)| e.size).sum();
    Ok(Preview {
        version: config.res_version,
        download_bytes: game_bytes + packed,
        install_bytes: game_bytes + launcher.iter().map(|e| e.size).sum::<u64>(),
        file_count: (chosen.count() + launcher.len()) as u64,
        approximate: false,
        parts: merge_parts(&entries),
    })
}

pub(super) async fn get_content_packs(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let requested = args
        .first()
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let game_id = match requested {
        Some(id) => id.to_string(),
        None => app.state::<BackendState>().config.active_game_id(),
    };
    let profile = game_profiles::profile(&game_id);
    let profile_id = game_profiles::profile_id(profile).to_string();

    if game_profiles::install_mode(profile) != Some("netease") {
        return Ok(ok_with(
            json!({ "gameId": profile_id, "packs": [], "supported": false }),
        ));
    }
    let config = match fetch_config(profile).await {
        Ok(config) => config,
        Err(e) => return Ok(err_response(offline_or(e).await)),
    };
    let list = match fetch_reslist(profile, &config).await {
        Ok(list) => list,
        Err(e) => return Ok(err_response(offline_or(e).await)),
    };

    let selected = game_profiles::content_tags(&profile_id);
    let packs: Vec<Value> = list
        .tag_totals()
        .into_iter()
        .filter(|(tag, _, _)| tag != BASE_TAG)
        .map(|(tag, bytes, files)| {
            let on = match selected.as_deref() {
                None => true,
                Some(list) => list.contains(&tag),
            };
            json!({
                "tag": tag,
                "language": tag_language(&tag),
                "bytes": bytes,
                "files": files,
                "selected": on
            })
        })
        .collect();

    Ok(ok_with(
        json!({ "gameId": profile_id, "packs": packs, "supported": true }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example/publish_PC";
    const MD5_A: &str = "0cc175b9c0f1b6a831c399e269772661";
    const MD5_B: &str = "92eb5ffee6ae2fec3ad71c777531578f";

    fn parsed(xml: &str) -> Vec<(String, Entry)> {
        parse_reslist(xml, BASE).unwrap().items
    }

    #[test]
    fn files_belong_to_the_group_they_are_listed_in_not_the_one_they_are_nested_in() {
        let xml = format!(
            r#"<ResList version="7">
            <Res filename="Base/a.pak" filesize="1" md5="{MD5_A}"/>
            <BaseVersion>
                <Res filename="Base/b.pak" filesize="2" md5="{MD5_A}"/>
            </BaseVersion>
            <BaseVersion tag="baseTag">
                <Res filename="Base/c.pak" filesize="3" md5="{MD5_A}"/>
            </BaseVersion>
            <BaseVersion tag="pakchunk102">
                <Res filename="Voice/en.pak" filesize="4" md5="{MD5_A}"/>
                <BaseVersion>
                    <Res filename="Voice/en2.pak" filesize="5" md5="{MD5_A}"/>
                </BaseVersion>
            </BaseVersion>
        </ResList>"#
        );
        let list = parse_reslist(&xml, BASE).unwrap();
        let tagged: Vec<(&str, &str)> = list.items.iter().map(|(tag, e)| (&*e.path, tag.as_str())).collect();
        assert_eq!(
            tagged,
            vec![
                ("Base/a.pak", BASE_TAG),
                ("Base/b.pak", BASE_TAG),
                ("Base/c.pak", BASE_TAG),
                ("Voice/en.pak", "pakchunk102"),
                ("Voice/en2.pak", "pakchunk102"),
            ]
        );
        assert_eq!(list.tag_totals(), vec![(BASE_TAG.to_string(), 6, 3), ("pakchunk102".to_string(), 9, 2)]);

        let picks = |picked: Option<&[String]>| manifest_of("7".into(), vec![], &list, vec![], picked);
        let all = picks(None);
        assert_eq!((all.entries.len(), all.groups.as_slice()), (5, &[BASE_TAG.to_string(), "pakchunk102".to_string()][..]));
        let base_only = picks(Some(&[]));
        assert_eq!((base_only.entries.len(), base_only.groups.as_slice()), (3, &[BASE_TAG.to_string()][..]));
    }

    #[test]
    fn a_list_becomes_whole_files_blocks_written_in_place_and_ranges_of_a_pak() {
        let xml = format!(
            r#"<ResList version="1">
            <Res filename="whole.bin" filesize="10" md5="{MD5_A}"/>
            <Res filename="empty.bin" filesize="0" md5="{MD5_A}"/>
            <Res filename="blocks.pak" filesize="10" md5="{MD5_A}">
                <Block start="5" size="5" md5="{MD5_B}"/>
                <Block start="0" size="5" md5="{MD5_A}"/>
            </Res>
            <Res filename="gap.pak" filesize="10" md5="{MD5_A}">
                <Block start="0" size="5" md5="{MD5_A}"/>
            </Res>
            <Pak md5="{MD5_B}" filesize="1000">
                <Entry name="\Content\x.uasset" offset="40" size="7" md5="{MD5_A}"/>
            </Pak>
        </ResList>"#
        );
        let sources: Vec<Source> = parsed(&xml).into_iter().map(|(_, e)| e.source).collect();
        let whole = format!("{BASE}/Res/0/{MD5_A}.10");
        assert_eq!(sources[0], Source::Url(whole.clone().into()));
        assert!(matches!(&sources[1], Source::Segments { parts, .. } if parts.is_empty()));
        match &sources[2] {
            Source::Segments { object, parts } => {
                assert_eq!(**object, *whole);
                assert_eq!(parts.iter().map(|p| (p.src, p.dst, p.size)).collect::<Vec<_>>(), vec![(0, 0, 5), (5, 5, 5)]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(sources[3], Source::Url(whole.into()), "blocks that leave a gap fall back to the whole file");
        let Source::Segments { object, parts } = &sources[4] else { panic!() };
        assert_eq!(**object, *format!("{BASE}/Res/9/{MD5_B}.1000"));
        assert_eq!((parts[0].src, parts[0].dst, parts[0].size), (40, 0, 7));
        assert_eq!(&*parsed(&xml)[4].1.path, "Content/x.uasset");
    }

    #[test]
    fn the_launcher_feed_lists_one_zip_per_runtime_file() {
        let xml = format!(
            r#"<Manifest><Url BaseUrl="https://dl.example/launcher/"/><ProductVersion Version="2.1"/>
            <File Path="bin\cef.dll" Size="9" Checksum="{MD5_A}" ZipSize="4" ZipChecksum="{MD5_B}"/>
            <File Path="qml/Flat/plugins.qmltypes" Size="0" Checksum="{MD5_A}" ZipSize="158" ZipChecksum="{MD5_B}"/></Manifest>"#
        );
        let files = parse_launcher_manifest(&xml).unwrap();
        assert_eq!(&*files[0].path, "NTEGlobal/bin/cef.dll");
        assert_eq!(files[0].url(), "https://dl.example/launcher/2.1/bin/cef.dll.zip");
        assert_eq!((files[0].size, files[0].fetch_size()), (9, 4));
        assert_eq!(files[0].hash, Hash::Md5(md5_bytes(MD5_A, "test").unwrap()));
        assert_eq!(files[1].hash, Hash::None, "an empty runtime file is checked by size, its listed checksum is wrong");
        assert!(parse_launcher_manifest("<Manifest/>").is_err());
    }

    #[test]
    fn a_pinned_container_still_decodes() {
        let raw = hex::decode(
            "50617463686572584d4c300017000000f396f7631f828281094c269cf67fd3ee745c86fb8d0e846fcf1e681eca909320",
        )
        .unwrap();
        assert_eq!(decode_container(&raw, &[0x11; 16]).unwrap(), b"<ResourceList kat='1'/>".to_vec());
        assert!(decode_container(&raw, &[0x12; 16]).is_err());
    }

    fn encoded(json: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(json)
    }

    #[test]
    fn an_old_branch_points_at_the_one_now_published() {
        let config = encoded(
            r#"{"appVersion":"0.0","gameResUrl":["https://ntecdn1.perfectworld.com/clientRes","https://ntecdn2.perfectworld.com/clientRes/"],"branchName":"PC_140","appId":"3000001"}"#,
        );
        let xml = format!(
            "<config><ResVersion>1.3.10</ResVersion><ResConfig>{config}</ResConfig><Extra><listHash>{MD5_A}</listHash></Extra></config>"
        );
        let feed = parse_config(&xml);
        assert_eq!(feed.res_version.as_deref(), Some("1.3.10"));
        assert_eq!(feed.list_hash.as_deref(), Some(MD5_A));
        assert_eq!(
            feed.redirect,
            Some(Redirect {
                branch: "PC_140".into(),
                roots: vec![
                    "https://ntecdn1.perfectworld.com/clientRes".into(),
                    "https://ntecdn2.perfectworld.com/clientRes".into()
                ],
            })
        );
        assert_eq!(
            bases_of(&feed.redirect.unwrap().roots, "PC_140"),
            vec![
                "https://ntecdn1.perfectworld.com/clientRes/PC_140".to_string(),
                "https://ntecdn2.perfectworld.com/clientRes/PC_140".to_string()
            ]
        );
    }

    #[test]
    fn the_published_branch_has_no_redirect_and_bad_ones_are_ignored() {
        let feed = parse_config(&format!("<config><ResVersion>1.4.5</ResVersion><listHash>{MD5_B}</listHash></config>"));
        assert_eq!(
            feed,
            Feed { res_version: Some("1.4.5".into()), list_hash: Some(MD5_B.into()), redirect: None }
        );
        for json in [
            r#"{"branchName":"../../etc"}"#,
            r#"{"branchName":""}"#,
            r#"{"gameResUrl":["https://x.example"]}"#,
        ] {
            assert_eq!(parse_redirect(&encoded(json)), None, "{json}");
        }
        assert_eq!(parse_redirect("not base64!"), None);
        let insecure = parse_redirect(&encoded(r#"{"branchName":"PC_150","gameResUrl":["http://x.example/clientRes"]}"#)).unwrap();
        assert!(insecure.roots.is_empty(), "only https roots are taken");
        assert_eq!(parse_config("<config/>"), Feed { res_version: None, list_hash: None, redirect: None });
    }
}
