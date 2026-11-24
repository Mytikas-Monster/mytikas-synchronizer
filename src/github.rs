use crate::archive::{self, ADDON, Files};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    env, fmt, fs,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const UPSTREAM: &str = "Mytikas-Monster/OlympusMytikas";
const FORK: &str = "Laymer/OlympusMytikas";
const API: &str = "https://api.github.com";

#[derive(Debug, PartialEq)]
pub enum Progress {
    Open(i64),
    Waiting(i64, String),
    Closed(i64),
    Published {
        pr: i64,
        commit: String,
        release_id: i64,
    },
    Blocked(String),
}

// Expected identity conflicts need human review. Network and API failures are retried.
#[derive(Debug)]
struct Conflict(&'static str);
impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Conflict {}

fn require(condition: bool, reason: &'static str) -> Result<()> {
    if !condition {
        return Err(Conflict(reason).into());
    }
    Ok(())
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("mytikas-synchronizer")
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(40))
        .build()?)
}

// All polling shares one process. Let the database cycle continue during a GitHub cooldown.
static RETRY_AT: AtomicU64 = AtomicU64::new(0);

fn retry_at(status: StatusCode, headers: &reqwest::header::HeaderMap, now: u64) -> Option<u64> {
    if !matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) {
        return None;
    }
    let number = |name: &str| headers.get(name)?.to_str().ok()?.parse::<u64>().ok();
    let retry = number("retry-after").map(|secs| now.saturating_add(secs));
    let reset = number("x-ratelimit-reset").filter(|_| number("x-ratelimit-remaining") == Some(0));
    Some(
        retry
            .into_iter()
            .chain(reset)
            .max()
            .unwrap_or_else(|| now.saturating_add(60)),
    )
}

trait GithubStatus {
    fn github_status(self) -> Result<reqwest::Response>;
}

impl GithubStatus for reqwest::Response {
    fn github_status(self) -> Result<reqwest::Response> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if let Some(deadline) = retry_at(self.status(), self.headers(), now) {
            RETRY_AT.fetch_max(deadline, Ordering::Relaxed);
        }
        Ok(self.error_for_status()?)
    }
}

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

struct App {
    client: Client,
    jwt: String,
    bot: String,
}

impl App {
    async fn authenticate() -> Result<Self> {
        let iss = env::var("GITHUB_APP_ID")?;
        let key = fs::read(env::var("GITHUB_APP_PRIVATE_KEY_FILE")?)
            .context("Unable to read GitHub App key")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(
            now >= RETRY_AT.load(Ordering::Relaxed),
            "GitHub rate limit cooldown is active"
        );
        let claims = Claims {
            iat: now - 60,
            exp: now + 540,
            iss,
        };
        let jwt = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(&key)?,
        )?;
        let client = client()?;
        #[derive(Deserialize)]
        struct Info {
            slug: String,
        }
        let info: Info = client
            .get(format!("{API}/app"))
            .bearer_auth(&jwt)
            .send()
            .await?
            .github_status()?
            .json()
            .await?;
        Ok(Self {
            client,
            jwt,
            bot: format!("{}[bot]", info.slug),
        })
    }

    async fn installation(&self, fork: bool) -> Result<Repo<'_>> {
        let (name, variable, permissions) = if fork {
            (
                FORK,
                "GITHUB_FORK_INSTALLATION_ID",
                json!({"contents":"write"}),
            )
        } else {
            (
                UPSTREAM,
                "GITHUB_UPSTREAM_INSTALLATION_ID",
                json!({"contents":"write","pull_requests":"write","actions":"read"}),
            )
        };
        let id = env::var(variable)?.parse::<u64>()?;
        #[derive(Deserialize)]
        struct Credentials {
            token: String,
        }
        let auth: Credentials = self
            .client
            .post(format!("{API}/app/installations/{id}/access_tokens"))
            .bearer_auth(&self.jwt)
            .json(&json!({"repositories":[ADDON],"permissions":permissions}))
            .send()
            .await?
            .github_status()?
            .json()
            .await?;
        Ok(Repo {
            base: format!("{API}/repos/{name}"),
            upload_base: format!("https://uploads.github.com/repos/{name}"),
            client: &self.client,
            token: auth.token,
        })
    }
}

struct Repo<'a> {
    base: String,
    upload_base: String,
    client: &'a Client,
    token: String,
}

impl Repo<'_> {
    async fn optional<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let response = self
            .client
            .get(format!("{}/{}", self.base, path))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.github_status()?.json().await?))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.optional(path)
            .await?
            .with_context(|| format!("GitHub endpoint unavailable: {path}"))
    }

    async fn write<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<T> {
        Ok(self
            .client
            .request(method, format!("{}/{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?
            .github_status()?
            .json()
            .await?)
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        self.write(Method::POST, path, body).await
    }

    async fn create_ref(&self, reference: &str, sha: &str) -> Result<()> {
        let _: GitRef = self
            .post("git/refs", json!({"ref": reference, "sha": sha}))
            .await?;
        Ok(())
    }

    async fn addon_tree(&self, commit: &str) -> Result<(String, Vec<Entry>)> {
        let commit: Commit = self.get(&format!("git/commits/{commit}")).await?;
        let root: Tree = self.get(&format!("git/trees/{}", commit.tree.sha)).await?;
        ensure!(!root.truncated, "Incomplete root tree");
        let subtree = root
            .tree
            .iter()
            .find(|e| e.path == ADDON && e.mode == "040000")
            .ok_or(Conflict(
                "The reviewed commit has no OlympusMytikas directory",
            ))?;
        let addon: Tree = self
            .get(&format!("git/trees/{}?recursive=1", subtree.sha))
            .await?;
        ensure!(!addon.truncated, "Incomplete addon tree");
        Ok((commit.tree.sha, addon.tree))
    }
}

#[derive(Deserialize)]
struct Object {
    sha: String,
}
#[derive(Deserialize)]
struct GitRef {
    object: GitTarget,
}
#[derive(Deserialize)]
struct GitTarget {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct Commit {
    tree: Object,
    message: String,
    parents: Vec<Object>,
}
#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
    truncated: bool,
}
#[derive(Deserialize)]
struct Entry {
    path: String,
    mode: String,
    sha: String,
}
#[derive(Deserialize)]
struct RepoName {
    full_name: String,
}
#[derive(Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    repo: Option<RepoName>,
}
#[derive(Deserialize)]
struct Actor {
    login: String,
}
#[derive(Deserialize)]
struct Pull {
    number: i64,
    user: Actor,
    head: Branch,
    base: Branch,
    state: String,
    merged_at: Option<String>,
    merge_commit_sha: Option<String>,
}
#[derive(Deserialize)]
struct Release {
    id: i64,
    author: Actor,
    tag_name: String,
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    id: i64,
    name: String,
    size: u64,
    state: String,
    digest: Option<String>,
}

struct Submission<'a> {
    id: i64,
    version: &'a str,
    notes: &'a str,
    sha256: &'a str,
    bytes: &'a [u8],
    files: Files,
}

impl Submission<'_> {
    fn branch(&self) -> String {
        format!(
            "archive-sync/{}-v{}-{}",
            self.id,
            self.version,
            &self.sha256[..12]
        )
    }
    fn tag(&self) -> String {
        format!("v{}", self.version)
    }
    fn marker(&self) -> String {
        format!(
            "Website archive #{} v{} SHA-256 {}",
            self.id, self.version, self.sha256
        )
    }
    fn asset(&self) -> String {
        format!("{ADDON}-{}.zip", self.version)
    }
}

fn matches_archive(tree: &[Entry], files: &Files) -> bool {
    let mut matched = 0;
    for entry in tree.iter().filter(|entry| entry.mode != "040000") {
        if entry.mode != "100644"
            || !files
                .get(&entry.path)
                .is_some_and(|file| file.git_sha == entry.sha)
        {
            return false;
        }
        matched += 1;
    }
    matched == files.len()
}

async fn prepare_branch(up: &Repo<'_>, fork: &Repo<'_>, sub: &Submission<'_>) -> Result<()> {
    let branch = sub.branch();
    // Never update an existing branch. A failed API call may already have created it.
    if let Some(reference) = fork
        .optional::<GitRef>(&format!("git/ref/heads/{branch}"))
        .await?
    {
        let head: Commit = fork
            .get(&format!("git/commits/{}", reference.object.sha))
            .await?;
        require(
            head.message == sub.marker() && head.parents.len() == 1,
            "Existing import branch has a different origin",
        )?;
        let (_, tree) = fork.addon_tree(&reference.object.sha).await?;
        require(
            matches_archive(&tree, &sub.files),
            "Existing import branch differs from the archive",
        )?;
        // Verify that a preexisting branch contains no changes outside the addon directory.
        #[derive(Deserialize)]
        struct Comparison {
            status: String,
        }
        let lineage: Comparison = up
            .get(&format!(
                "compare/{}...main?per_page=1",
                head.parents[0].sha
            ))
            .await?;
        require(
            matches!(lineage.status.as_str(), "identical" | "ahead"),
            "Import branch parent is outside the upstream main history",
        )?;
        let parent: Commit = fork
            .get(&format!("git/commits/{}", head.parents[0].sha))
            .await?;
        let old: Tree = fork.get(&format!("git/trees/{}", parent.tree.sha)).await?;
        let new: Tree = fork.get(&format!("git/trees/{}", head.tree.sha)).await?;
        require(
            !old.truncated && !new.truncated && siblings(&old) == siblings(&new),
            "Import branch modified files outside OlympusMytikas",
        )?;
        return Ok(());
    }
    let base: GitRef = up.get("git/ref/heads/main").await?;
    let (base_tree, old) = up.addon_tree(&base.object.sha).await?;
    require(
        !matches_archive(&old, &sub.files),
        "The addon source is already present on main without this submission PR",
    )?;
    let mut known: std::collections::HashSet<_> = old
        .iter()
        .filter(|e| e.mode == "100644")
        .map(|e| e.sha.clone())
        .collect();
    let mut entries = Vec::with_capacity(sub.files.len());
    let mut inline_budget = 512_000;
    for (name, file) in &sub.files {
        let mut entry = json!({"path":name,"sha":file.git_sha,"mode":"100644","type":"blob"});
        if known.insert(file.git_sha.clone()) {
            // Batch changed source into one tree request. Keep binary bytes in Git blobs.
            if file.data.len() <= inline_budget
                && !file.data.contains(&0)
                && let Ok(text) = std::str::from_utf8(&file.data)
            {
                inline_budget -= file.data.len();
                entry = json!({"path":name,"content":text,"mode":"100644","type":"blob"});
                // Another path may reference these bytes before the tree has created them.
                known.remove(&file.git_sha);
            } else {
                let response = fork
                    .client
                    .head(format!("{}/git/blobs/{}", fork.base, file.git_sha))
                    .bearer_auth(&fork.token)
                    .send()
                    .await?;
                if response.status() == StatusCode::NOT_FOUND {
                    let blob: Object = fork
                        .post(
                            "git/blobs",
                            json!({"encoding":"base64","content":STANDARD.encode(&file.data)}),
                        )
                        .await?;
                    ensure!(blob.sha == file.git_sha, "GitHub blob hash mismatch");
                } else {
                    response.github_status()?;
                }
            }
        }
        entries.push(entry);
    }
    // A fresh subtree records deletions while keeping every sibling from upstream.
    let addon: Object = fork.post("git/trees", json!({"tree":entries})).await?;
    let new_root = json!({
        "base_tree": base_tree,
        "tree": [{"path": ADDON, "mode": "040000", "type": "tree", "sha": addon.sha}]
    });
    let root: Object = fork.post("git/trees", new_root).await?;
    let commit: Object = fork
        .post(
            "git/commits",
            json!({"message":sub.marker(),"tree":root.sha,"parents":[base.object.sha]}),
        )
        .await?;
    fork.create_ref(&format!("refs/heads/{branch}"), &commit.sha)
        .await
}

fn siblings(tree: &Tree) -> HashMap<&str, (&str, &str)> {
    tree.tree
        .iter()
        .filter(|e| e.path != ADDON)
        .map(|e| (e.path.as_str(), (e.mode.as_str(), e.sha.as_str())))
        .collect()
}

async fn pull(up: &Repo<'_>, app: &App, sub: &Submission<'_>, pr: Option<i64>) -> Result<Pull> {
    let branch = sub.branch();
    if let Some(number) = pr {
        return up.get(&format!("pulls/{number}")).await;
    }
    let mut existing: Vec<Pull> = up
        .get(&format!("pulls?state=all&base=main&head=Laymer:{branch}"))
        .await?;
    require(
        existing.len() <= 1,
        "Multiple PRs for the same archive branch",
    )?;
    if let Some(pr) = existing.pop() {
        return Ok(pr);
    }
    let tag = sub.tag();
    require(
        up.optional::<GitRef>(&format!("git/ref/tags/{tag}"))
            .await?
            .is_none(),
        "Version tag already exists",
    )?;
    let fork = app.installation(true).await?;
    prepare_branch(up, &fork, sub).await?;
    let body = format!(
        "## What changed for the player?\n\n{}\n\n## How did you verify it?\n\nValidated the uploaded ZIP and manifest. {}. Review the addon changes and Addon behavior CI before merging.",
        sub.notes,
        sub.marker()
    );
    up.post("pulls", json!({"base":"main","head":format!("Laymer:{branch}"),"title":format!("Import Olympus Mytikas {tag}"),"body":body,"maintainer_can_modify":false})).await
}

fn verify_pr(pr: &Pull, bot: &str, branch: &str) -> Result<()> {
    require(
        pr.user.login == bot
            && pr.base.name == "main"
            && pr
                .base
                .repo
                .as_ref()
                .is_some_and(|r| r.full_name == UPSTREAM)
            && pr.head.name == branch
            && pr.head.repo.as_ref().is_some_and(|r| r.full_name == FORK),
        "Pull request identity differs from the recorded import",
    )
}

async fn ci_success(up: &Repo<'_>, commit: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct Runs {
        workflow_runs: Vec<Run>,
    }
    #[derive(Deserialize)]
    struct Run {
        head_sha: String,
        event: String,
        head_branch: String,
        status: String,
        conclusion: Option<String>,
    }
    // A main push tests the actual merge result, including squashed and rebased PRs.
    let runs:Runs = up.get(&format!("actions/workflows/shared-window.yml/runs?branch=main&event=push&head_sha={commit}&per_page=1")).await?;
    let Some(run) = runs.workflow_runs.first() else {
        return Ok(false);
    };
    ensure!(
        run.head_sha == commit && run.event == "push" && run.head_branch == "main",
        "Unexpected workflow result"
    );
    if run.status != "completed" {
        return Ok(false);
    }
    require(
        run.conclusion.as_deref() == Some("success"),
        "Addon behavior did not pass on the merged commit. Rerun the workflow, then resume this submission after it passes.",
    )?;
    Ok(true)
}

async fn tag_merge(up: &Repo<'_>, sub: &Submission<'_>, commit: &str) -> Result<()> {
    let tag = sub.tag();
    if let Some(tag_ref) = up
        .optional::<GitRef>(&format!("git/ref/tags/{tag}"))
        .await?
    {
        require(
            tag_ref.object.kind == "tag",
            "Existing tag does not identify this import",
        )?;
        #[derive(Deserialize)]
        struct Tag {
            tag: String,
            message: String,
            object: GitTarget,
        }
        let existing: Tag = up.get(&format!("git/tags/{}", tag_ref.object.sha)).await?;
        return require(
            existing.tag == tag
                && existing.message.trim_end() == sub.marker()
                && existing.object.kind == "commit"
                && existing.object.sha == commit,
            "Existing tag belongs to a different archive or commit",
        );
    }
    let object: Object = up
        .post(
            "git/tags",
            json!({"tag":tag,"message":sub.marker(),"object":commit,"type":"commit"}),
        )
        .await?;
    up.create_ref(&format!("refs/tags/{tag}"), &object.sha)
        .await
}

async fn find_release(up: &Repo<'_>, tag: &str) -> Result<Option<Release>> {
    if let Some(release) = up.optional(&format!("releases/tags/{tag}")).await? {
        return Ok(Some(release));
    }
    // The by-tag endpoint can omit drafts. Include authenticated draft listings on recovery.
    let mut page = 1;
    loop {
        let releases: Vec<Release> = up
            .get(&format!("releases?per_page=100&page={page}"))
            .await?;
        let last = releases.len() < 100;
        if let Some(release) = releases.into_iter().find(|r| r.tag_name == tag) {
            return Ok(Some(release));
        }
        if last {
            return Ok(None);
        }
        page += 1;
    }
}

fn asset_matches(asset: &Asset, sub: &Submission<'_>) -> bool {
    asset.name == sub.asset()
        && asset.state == "uploaded"
        && asset.size == sub.bytes.len() as u64
        && asset.digest.as_deref() == Some(&format!("sha256:{}", sub.sha256))
}

async fn publish(up: &Repo<'_>, sub: &Submission<'_>, bot: &str, commit: &str) -> Result<i64> {
    let tag = sub.tag();
    let release = match find_release(up, &tag).await? {
        Some(release) => release,
        None => {
            let draft = json!({
                "tag_name": tag, "target_commitish": commit, "name": format!("Olympus Mytikas {tag}"),
                "body": sub.notes, "draft": true, "prerelease": false
            });
            up.post("releases", draft).await?
        }
    };
    require(
        release.author.login == bot
            && !release.prerelease
            && release.tag_name == tag
            && release.body.as_deref() == Some(sub.notes),
        "Existing release belongs to a different publication",
    )?;
    if let Some(asset) = release.assets.iter().find(|a| a.name == sub.asset()) {
        // GitHub can retain an empty placeholder after a failed asset upload.
        if release.draft && asset.state == "starter" && asset.size == 0 && asset.digest.is_none() {
            up.client
                .delete(format!("{}/releases/assets/{}", up.base, asset.id))
                .bearer_auth(&up.token)
                .send()
                .await?
                .github_status()?;
            bail!("Removed incomplete upload from the draft; retrying the original ZIP");
        }
        require(
            asset_matches(asset, sub),
            "Release asset differs from the original ZIP",
        )?;
    } else {
        require(
            release.draft,
            "Published release is missing the original ZIP",
        )?;
        let url = format!("{}/releases/{}/assets", up.upload_base, release.id);
        let asset: Asset = up
            .client
            .post(url)
            .query(&[("name", sub.asset())])
            .bearer_auth(&up.token)
            .header("Content-Type", "application/zip")
            .body(sub.bytes.to_vec())
            .send()
            .await?
            .github_status()?
            .json()
            .await?;
        require(
            asset_matches(&asset, sub),
            "GitHub did not store the original ZIP",
        )?;
    }
    if release.draft {
        let published: Release = up
            .write(
                Method::PATCH,
                &format!("releases/{}", release.id),
                json!({"draft":false}),
            )
            .await?;
        ensure!(
            !published.draft
                && published.id == release.id
                && published.assets.iter().any(|a| asset_matches(a, sub)),
            "Release publication could not be verified"
        );
    }
    Ok(release.id)
}

async fn synchronize(sub: &Submission<'_>, pr: Option<i64>) -> Result<Progress> {
    let app = App::authenticate().await?;
    let up = app.installation(false).await?;
    let pull = pull(&up, &app, sub, pr).await?;
    after_review(&up, sub, &app.bot, pull).await
}

async fn after_review(
    up: &Repo<'_>,
    sub: &Submission<'_>,
    bot: &str,
    pull: Pull,
) -> Result<Progress> {
    verify_pr(&pull, bot, &sub.branch())?;
    if pull.state == "open" {
        return Ok(Progress::Open(pull.number));
    }
    ensure!(
        pull.state == "closed",
        "Unexpected GitHub pull request state"
    );
    if pull.merged_at.is_none() {
        return Ok(Progress::Closed(pull.number));
    }
    let commit = pull
        .merge_commit_sha
        .context("Merged PR has no commit SHA")?;
    let (_, tree) = up.addon_tree(&commit).await?;
    require(
        matches_archive(&tree, &sub.files),
        "Merged addon files differ from the original ZIP",
    )?;
    if !ci_success(up, &commit).await? {
        return Ok(Progress::Waiting(
            pull.number,
            "Waiting for Addon behavior on the merged commit.".into(),
        ));
    }
    tag_merge(up, sub, &commit).await?;
    let release_id = publish(up, sub, bot, &commit).await?;
    Ok(Progress::Published {
        pr: pull.number,
        commit,
        release_id,
    })
}

pub async fn advance(
    id: i64,
    version: &str,
    notes: &str,
    sha256: &str,
    bytes: &[u8],
    pr: Option<i64>,
) -> Result<Progress> {
    if notes.trim().is_empty() {
        return Ok(Progress::Blocked(
            "Public release notes are required".into(),
        ));
    }
    let files = match archive::inspect(bytes, version, sha256) {
        Ok(files) => files,
        Err(e) => return Ok(Progress::Blocked(format!("Invalid archive: {e}"))),
    };
    let sub = Submission {
        id,
        version,
        notes,
        sha256,
        bytes,
        files,
    };
    match synchronize(&sub, pr).await {
        Err(e) if e.is::<Conflict>() => Ok(Progress::Blocked(e.to_string())),
        result => result,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    project_id: i64,
    release_tag: String,
    asset_name: String,
    sha256: String,
    file_id: i64,
}

fn verify_receipt(bytes: &[u8], version: &str, sha256: &str) -> Result<i64> {
    let receipt: Receipt = serde_json::from_slice(bytes)?;
    ensure!(
        receipt.project_id == 1822931
            && receipt.release_tag == format!("v{version}")
            && receipt.asset_name == format!("{ADDON}-{version}.zip")
            && receipt.sha256 == sha256
            && receipt.file_id > 0,
        "CurseForge receipt identifies another file or project"
    );
    Ok(receipt.file_id)
}

pub async fn receipt(version: &str, sha256: &str) -> Result<Option<i64>> {
    ensure!(archive::stable_version(version), "Invalid receipt version");
    let app = App::authenticate().await?;
    let up = app.installation(false).await?;
    let release: Release = up.get(&format!("releases/tags/v{version}")).await?;
    let Some(asset) = release
        .assets
        .iter()
        .find(|a| a.name == "curseforge-receipt.json")
    else {
        return Ok(None);
    };
    ensure!(
        asset.size <= 8192 && asset.state == "uploaded",
        "Invalid receipt asset"
    );
    let url = format!(
        "https://github.com/{UPSTREAM}/releases/download/v{version}/curseforge-receipt.json"
    );
    let mut response = up.client.get(url).send().await?.github_status()?;
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if data.len() + chunk.len() > 8192 {
            bail!("CurseForge receipt is too large");
        }
        data.extend_from_slice(&chunk);
    }
    ensure!(
        asset.digest.as_deref() == Some(&format!("sha256:{:x}", Sha256::digest(&data))),
        "Receipt asset digest mismatch"
    );
    Ok(Some(verify_receipt(&data, version, sha256)?))
}

pub async fn preflight() -> Result<()> {
    #[derive(Deserialize)]
    struct InstallationRepos {
        repositories: Vec<RepoName>,
    }
    #[derive(Deserialize)]
    struct Info {
        full_name: String,
        default_branch: String,
        parent: Option<RepoName>,
    }
    #[derive(Deserialize)]
    struct Workflow {
        state: String,
    }

    let app = App::authenticate().await?;
    for (name, fork) in [(UPSTREAM, false), (FORK, true)] {
        let repo = app.installation(fork).await?;
        let access: InstallationRepos = repo
            .client
            .get(format!("{API}/installation/repositories"))
            .bearer_auth(&repo.token)
            .send()
            .await?
            .github_status()?
            .json()
            .await?;
        ensure!(
            access.repositories.len() == 1 && access.repositories[0].full_name == name,
            "Installation must grant access to {name}"
        );
        let info: Info = repo
            .client
            .get(&repo.base)
            .bearer_auth(&repo.token)
            .send()
            .await?
            .github_status()?
            .json()
            .await?;
        ensure!(
            info.full_name == name && info.default_branch == "main",
            "Unexpected repository identity or default branch for {name}"
        );
        if fork {
            ensure!(
                info.parent
                    .as_ref()
                    .is_some_and(|parent| parent.full_name == UPSTREAM),
                "Runtime fork must be a direct fork of {UPSTREAM}"
            );
        } else {
            for file in ["shared-window.yml", "publish-curseforge.yml"] {
                let workflow: Workflow = repo.get(&format!("actions/workflows/{file}")).await?;
                ensure!(
                    workflow.state == "active",
                    "{file} must be active in {UPSTREAM}"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    fn submission() -> Submission<'static> {
        Submission {
            id: 1,
            version: "1.2.3",
            notes: "Public notes.",
            sha256: "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            bytes: b"hello world",
            files: Files::new(),
        }
    }

    fn repo<'a>(server: &Server, client: &'a Client) -> Repo<'a> {
        Repo {
            base: server.url(),
            upload_base: server.url(),
            client,
            token: "fixture".into(),
        }
    }

    fn merged(sub: &Submission<'_>) -> Pull {
        serde_json::from_value(
            json!({"number":7,"state":"closed","merged_at":"time","merge_commit_sha":"merge",
            "user":{"login":"test[bot]"},"base":{"ref":"main","repo":{"full_name":UPSTREAM}},
            "head":{"ref":sub.branch(),"repo":{"full_name":FORK}}}),
        )
        .unwrap()
    }

    fn release(sub: &Submission<'_>, draft: bool, assets: Value) -> Value {
        json!({"id":10,"author":{"login":"test[bot]"},"draft":draft,"prerelease":false,
            "tag_name":sub.tag(),"body":sub.notes,"assets":assets})
    }

    #[tokio::test]
    async fn merged_import_reuses_tag_and_original_asset_after_interrupted_publish() {
        let mut server = Server::new_async().await;
        let client = client().unwrap();
        let repo = repo(&server, &client);
        let sub = submission();
        let tree = server
            .mock("GET", "/git/commits/merge")
            .with_body(json!({"tree":{"sha":"root"},"parents":[],"message":"Merge"}).to_string())
            .expect(3)
            .create_async()
            .await;
        let _root = server
            .mock("GET", "/git/trees/root")
            .with_body(
                json!({"truncated":false,"tree":[{"path":ADDON,"mode":"040000","sha":"addon"}]})
                    .to_string(),
            )
            .create_async()
            .await;
        let _addon = server
            .mock("GET", "/git/trees/addon")
            .match_query("recursive=1")
            .with_body(r#"{"truncated":false,"tree":[]}"#)
            .create_async()
            .await;
        let run = json!({"head_sha":"merge","event":"push","head_branch":"main","status":"completed","conclusion":"success"});
        let _ci = server
            .mock("GET", "/actions/workflows/shared-window.yml/runs")
            .match_query("branch=main&event=push&head_sha=merge&per_page=1")
            .with_body(json!({"workflow_runs":[run]}).to_string())
            .create_async()
            .await;
        let tag_absent = server
            .mock("GET", "/git/ref/tags/v1.2.3")
            .with_status(404)
            .create_async()
            .await;
        let tag = server
            .mock("POST", "/git/tags")
            .match_body(Matcher::Json(
                json!({"tag":sub.tag(),"message":sub.marker(),"type":"commit","object":"merge"}),
            ))
            .with_body(r#"{"sha":"annotated"}"#)
            .create_async()
            .await;
        let tag_ref = json!({"object":{"sha":"annotated","type":"tag"}});
        let reference = server
            .mock("POST", "/git/refs")
            .match_body(Matcher::Json(
                json!({"ref":"refs/tags/v1.2.3","sha":"annotated"}),
            ))
            .with_body(tag_ref.to_string())
            .create_async()
            .await;
        let release_absent = server
            .mock("GET", "/releases/tags/v1.2.3")
            .with_status(404)
            .create_async()
            .await;
        let _drafts = server
            .mock("GET", "/releases")
            .match_query("per_page=100&page=1")
            .with_body("[]")
            .create_async()
            .await;
        let draft = server
            .mock("POST", "/releases")
            .match_body(Matcher::PartialJson(
                json!({"draft":true,"target_commitish":"merge","body":sub.notes}),
            ))
            .with_body(release(&sub, true, json!([])).to_string())
            .create_async()
            .await;
        let asset = json!({"id":21,"name":sub.asset(),"size":11,"state":"uploaded","digest":format!("sha256:{}",sub.sha256)});
        let upload = server
            .mock("POST", "/releases/10/assets")
            .match_query("name=OlympusMytikas-1.2.3.zip")
            .match_header("Content-Type", "application/zip")
            .match_body(sub.bytes.to_vec())
            .with_body(asset.to_string())
            .create_async()
            .await;
        let fail = server
            .mock("PATCH", "/releases/10")
            .with_status(503)
            .create_async()
            .await;
        assert!(
            after_review(&repo, &sub, "test[bot]", merged(&sub))
                .await
                .is_err()
        );
        fail.assert_async().await;
        drop((tag_absent, release_absent, fail));

        let _existing_tag = server
            .mock("GET", "/git/ref/tags/v1.2.3")
            .with_body(tag_ref.to_string())
            .create_async()
            .await;
        let _existing_obj=server.mock("GET","/git/tags/annotated").with_body(json!({"tag":sub.tag(),"message":sub.marker(),"object":{"type":"commit","sha":"merge"}}).to_string()).create_async().await;
        let already_uploaded = server
            .mock("GET", "/releases/tags/v1.2.3")
            .with_body(release(&sub, true, json!([asset])).to_string())
            .create_async()
            .await;
        let public = release(&sub, false, json!([asset]));
        let publish = server
            .mock("PATCH", "/releases/10")
            .match_body(Matcher::Json(json!({"draft":false})))
            .with_body(public.to_string())
            .create_async()
            .await;
        let expected = Progress::Published {
            pr: 7,
            commit: "merge".into(),
            release_id: 10,
        };
        assert_eq!(
            after_review(&repo, &sub, "test[bot]", merged(&sub))
                .await
                .unwrap(),
            expected
        );
        drop(already_uploaded);
        let _live = server
            .mock("GET", "/releases/tags/v1.2.3")
            .with_body(public.to_string())
            .create_async()
            .await;
        assert_eq!(
            after_review(&repo, &sub, "test[bot]", merged(&sub))
                .await
                .unwrap(),
            expected
        );
        for mock in [tree, tag, reference, draft, upload, publish] {
            mock.assert_async().await;
        }
    }

    #[tokio::test]
    async fn closed_open_and_mismatched_review_never_release() {
        let server = Server::new_async().await;
        let client = client().unwrap();
        let repo = repo(&server, &client);
        let sub = submission();
        let mut pr = merged(&sub);
        pr.merged_at = None;
        assert_eq!(
            after_review(&repo, &sub, "test[bot]", pr).await.unwrap(),
            Progress::Closed(7)
        );
        let mut pr = merged(&sub);
        pr.state = "open".into();
        assert_eq!(
            after_review(&repo, &sub, "test[bot]", pr).await.unwrap(),
            Progress::Open(7)
        );
        assert!(
            after_review(&repo, &sub, "someone", merged(&sub))
                .await
                .unwrap_err()
                .is::<Conflict>()
        );
    }

    #[tokio::test]
    async fn removes_only_an_empty_starter_from_our_draft() {
        let mut server = Server::new_async().await;
        let client = client().unwrap();
        let repo = repo(&server, &client);
        let sub = submission();
        let asset = json!({"id":21,"name":sub.asset(),"size":0,"state":"starter","digest":null});
        let delete = server
            .mock("DELETE", "/releases/assets/21")
            .with_status(204)
            .expect(1)
            .create_async()
            .await;
        for (draft, size, state, digest, owned, cleanup) in [
            (false, 0, "starter", None, true, false),
            (true, 1, "starter", None, true, false),
            (true, 0, "starter", Some("another"), true, false),
            (true, 0, "uploaded", None, true, false),
            (true, 0, "starter", None, false, false),
            (true, 0, "starter", None, true, true),
        ] {
            let mut a = asset.clone();
            a["size"] = json!(size);
            a["state"] = json!(state);
            a["digest"] = json!(digest);
            let mut value = release(&sub, draft, json!([a]));
            if !owned {
                value["author"]["login"] = json!("someone");
            }
            let _release = server
                .mock("GET", "/releases/tags/v1.2.3")
                .with_body(value.to_string())
                .create_async()
                .await;
            let err = publish(&repo, &sub, "test[bot]", "merge")
                .await
                .unwrap_err();
            assert_eq!(err.is::<Conflict>(), !cleanup);
        }
        delete.assert_async().await;
    }

    #[tokio::test]
    async fn fork_commit_replaces_only_addon_tree() {
        let mut upstream = Server::new_async().await;
        let mut fork = Server::new_async().await;
        let client = client().unwrap();
        let up = repo(&upstream, &client);
        let target = repo(&fork, &client);
        let mut sub = submission();
        sub.files.insert(
            "new.lua".into(),
            archive::SourceFile {
                git_sha: "new".into(),
                data: b"abc\r\n".to_vec(),
            },
        );
        let branch = format!("/git/ref/heads/{}", sub.branch());
        let _absent = fork
            .mock("GET", branch.as_str())
            .with_status(404)
            .create_async()
            .await;
        let _base = upstream
            .mock("GET", "/git/ref/heads/main")
            .with_body(r#"{"object":{"type":"commit","sha":"base"}}"#)
            .create_async()
            .await;
        let _commit = upstream
            .mock("GET", "/git/commits/base")
            .with_body(r#"{"message":"base","tree":{"sha":"root"},"parents":[]}"#)
            .create_async()
            .await;
        let _root = upstream
            .mock("GET", "/git/trees/root")
            .with_body(
                json!({"truncated":false,"tree":[{"path":ADDON,"mode":"040000","sha":"addon"}]})
                    .to_string(),
            )
            .create_async()
            .await;
        let _old = upstream
            .mock("GET", "/git/trees/addon")
            .match_query("recursive=1")
            .with_body(
                r#"{"truncated":false,"tree":[{"path":"gone.lua","mode":"100644","sha":"old"}]}"#,
            )
            .create_async()
            .await;
        let subtree=fork.mock("POST","/git/trees").match_body(Matcher::Json(json!({"tree":[{"mode":"100644","path":"new.lua","type":"blob","content":"abc\r\n"}]})))
            .with_body(r#"{"sha":"snapshot"}"#).create_async().await;
        let root=fork.mock("POST","/git/trees").match_body(Matcher::Json(json!({"base_tree":"root","tree":[{"mode":"040000","path":ADDON,"type":"tree","sha":"snapshot"}]})))
            .with_body(r#"{"sha":"new-root"}"#).create_async().await;
        let commit = fork
            .mock("POST", "/git/commits")
            .match_body(Matcher::Json(
                json!({"message":sub.marker(),"tree":"new-root","parents":["base"]}),
            ))
            .with_body(r#"{"sha":"result"}"#)
            .create_async()
            .await;
        let create = fork
            .mock("POST", "/git/refs")
            .match_body(Matcher::Json(
                json!({"ref":format!("refs/heads/{}",sub.branch()),"sha":"result"}),
            ))
            .with_body(r#"{"object":{"type":"commit","sha":"result"}}"#)
            .create_async()
            .await;
        prepare_branch(&up, &target, &sub).await.unwrap();
        for mock in [subtree, root, commit, create] {
            mock.assert_async().await;
        }
    }

    #[test]
    fn honors_github_cooldowns() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "20".parse().unwrap());
        headers.insert("x-ratelimit-reset", "4000".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "3".parse().unwrap());
        assert_eq!(
            retry_at(StatusCode::TOO_MANY_REQUESTS, &headers, 100),
            Some(120)
        );
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert_eq!(retry_at(StatusCode::FORBIDDEN, &headers, 100), Some(4000));
        assert_eq!(retry_at(StatusCode::OK, &headers, 100), None);
    }

    #[test]
    fn receipt_identifies_the_uploaded_project_and_bytes() {
        let sub = submission();
        let good = json!({"projectId":1822931,"releaseTag":sub.tag(),"assetName":sub.asset(),"sha256":sub.sha256,"fileId":123});
        assert_eq!(
            verify_receipt(good.to_string().as_bytes(), sub.version, sub.sha256).unwrap(),
            123
        );
        for field in ["projectId", "releaseTag", "assetName", "sha256", "fileId"] {
            let mut invalid = good.clone();
            invalid[field] = json!(0);
            assert!(
                verify_receipt(invalid.to_string().as_bytes(), sub.version, sub.sha256).is_err()
            );
        }
    }
}
