//! `ImagesService` — HYPERFORGE-59: container images on an org's registry.
//!
//! ghcr.io and Codeberg: list, list every image, push, delete. Push and delete only say what they would do until `--execute true`.

use std::path::PathBuf;

use async_stream::stream;
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v5::ops::images as ops;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImagesEvent {
    /// `list_all`: one image the org has.
    ImagePackage {
        registry: String,
        owner: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        visibility: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    /// `list`: one version of an image, with every tag it carries.
    ImageVersion {
        registry: String,
        image: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
        tags: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    /// `list`, `list_all`: how many were listed.
    ImagesListed { registry: String, owner: String, count: u32 },
    /// `push`: one reference, as planned or as pushed.
    ImagePush { image: String, source: String, pushed: bool, dry_run: bool },
    /// `delete`: the version a tag or digest names, as planned or deleted.
    ImageDelete {
        image: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
        /// Tags that go with it (one image under several tags).
        other_tags: Vec<String>,
        deleted: bool,
        dry_run: bool,
    },
    Note { message: String },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
}

fn err(code: &str, message: impl Into<String>) -> ImagesEvent {
    ImagesEvent::Error { code: Some(code.into()), message: message.into() }
}

fn endpoint() -> Option<String> {
    std::env::var("HF_REGISTRY_API_URL").ok()
}

#[derive(Clone)]
pub struct ImagesService {
    config_dir: PathBuf,
}

impl ImagesService {
    #[must_use]
    pub const fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
}

#[plexus_macros::activation(
    trait = "ImagesApi",
    methods = marked,
    namespace = "images",
    description = "Container images on an org's forge registry (ghcr.io, Codeberg)",
    crate_path = "plexus_core"
)]
impl ImagesService {
    /// Every version of one image, with its tags and digest.
    #[plexus_macros::method(params(
        org = "Org (the registry owner)",
        name = "Image name (ghcr.io/<org>/<name>)",
        forge = "github | codeberg (default: the org's forge)",
        filter = "Only versions with a tag containing this text"
    ))]
    pub async fn list(
        &self,
        org: String,
        name: String,
        forge: Option<String>,
        filter: Option<String>,
    ) -> impl Stream<Item = ImagesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let registry = match ops::registry_for(&config_dir, &org, forge.as_deref()) {
                Ok(r) => r, Err((c, m)) => { yield err(c, m); return; }
            };
            let api = ops::Api::new(&config_dir, registry, &org, endpoint());
            let versions = match api.versions(&name).await {
                Ok(v) => v, Err((c, m)) => { yield err(c, m); return; }
            };
            let image = format!("{}/{org}/{name}", registry.host());
            let mut count = 0u32;
            for v in versions {
                if let Some(f) = &filter {
                    if !v.tags.iter().any(|t| t.contains(f.as_str())) { continue; }
                }
                count += 1;
                yield ImagesEvent::ImageVersion {
                    registry: registry.host().into(), image: image.clone(),
                    digest: v.digest, tags: v.tags, created_at: v.created_at,
                };
            }
            yield ImagesEvent::ImagesListed { registry: registry.host().into(), owner: org, count };
        }
    }

    /// Every image the org has on its registry.
    #[plexus_macros::method(params(
        org = "Org (the registry owner)",
        forge = "github | codeberg (default: the org's forge)",
        filter = "Only images whose name contains this text"
    ))]
    pub async fn list_all(
        &self,
        org: String,
        forge: Option<String>,
        filter: Option<String>,
    ) -> impl Stream<Item = ImagesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let registry = match ops::registry_for(&config_dir, &org, forge.as_deref()) {
                Ok(r) => r, Err((c, m)) => { yield err(c, m); return; }
            };
            let api = ops::Api::new(&config_dir, registry, &org, endpoint());
            let packages = match api.packages().await {
                Ok(p) => p, Err((c, m)) => { yield err(c, m); return; }
            };
            let mut count = 0u32;
            for p in packages {
                if let Some(f) = &filter {
                    if !p.name.contains(f.as_str()) { continue; }
                }
                count += 1;
                yield ImagesEvent::ImagePackage {
                    registry: registry.host().into(), owner: org.clone(),
                    name: p.name, visibility: p.visibility, created_at: p.created_at,
                };
            }
            yield ImagesEvent::ImagesListed { registry: registry.host().into(), owner: org, count };
        }
    }

    /// Build an image from a directory (or take one built here) and push it.
    #[plexus_macros::method(params(
        org = "Org (the registry owner)",
        name = "Image name (ghcr.io/<org>/<name>)",
        tag = "Tag to push",
        also_tags = "More tags for the same image, e.g. latest",
        path = "Build context holding a Dockerfile (or pass image)",
        dockerfile = "Dockerfile, relative to path (default: Dockerfile, Containerfile, docker/Dockerfile)",
        image = "An image already built here to push (or pass path)",
        forge = "github | codeberg (default: the org's forge)",
        execute = "Build and push (default false: check and list only)"
    ))]
    #[allow(clippy::too_many_arguments)]
    pub async fn push(
        &self,
        org: String,
        name: String,
        tag: String,
        also_tags: Option<Vec<String>>,
        path: Option<String>,
        dockerfile: Option<String>,
        image: Option<String>,
        forge: Option<String>,
        execute: Option<bool>,
    ) -> impl Stream<Item = ImagesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let execute = execute.unwrap_or(false);
        stream! {
            let registry = match ops::registry_for(&config_dir, &org, forge.as_deref()) {
                Ok(r) => r, Err((c, m)) => { yield err(c, m); return; }
            };
            let mut tags = vec![tag];
            for t in also_tags.unwrap_or_default() {
                if !tags.contains(&t) { tags.push(t); }
            }
            let args = ops::PushArgs {
                owner: org.clone(), registry, name, tags,
                context: path.map(PathBuf::from), dockerfile, image, cli: None,
            };
            let plan = match ops::plan_push(args).await {
                Ok(p) => p, Err((c, m)) => { yield err(c, m); return; }
            };
            let source = plan.source.describe();
            if !execute {
                for t in &plan.targets {
                    yield ImagesEvent::ImagePush { image: t.clone(), source: source.clone(), pushed: false, dry_run: true };
                }
                return;
            }
            let api = ops::Api::new(&config_dir, registry, &org, endpoint());
            let token = ops::token(&config_dir, registry, &org);
            let login = if let Some(token) = token {
                match api.login_name().await {
                    Ok(user) => Some((user, token)),
                    Err((c, m)) => { yield err(c, m); return; }
                }
            } else {
                yield ImagesEvent::Note { message: format!("no token for {org} on {}: using the container CLI's stored login", registry.forge()) };
                None
            };
            let mut pushed = Vec::new();
            let result = plan.execute(login, |t| pushed.push(t.to_string())).await;
            for t in &pushed {
                yield ImagesEvent::ImagePush { image: t.clone(), source: source.clone(), pushed: true, dry_run: false };
            }
            if let Err((c, m)) = result {
                yield err(c, m);
            }
        }
    }

    /// Delete the image version a tag (or `sha256:` digest) names.
    #[plexus_macros::method(params(
        org = "Org (the registry owner)",
        name = "Image name",
        tag = "Tag, or sha256: digest, of the version to delete",
        with_other_tags = "Also delete when the version carries other tags (default false: refused)",
        forge = "github | codeberg (default: the org's forge)",
        execute = "Delete (default false: say what would go)"
    ))]
    pub async fn delete(
        &self,
        org: String,
        name: String,
        tag: String,
        with_other_tags: Option<bool>,
        forge: Option<String>,
        execute: Option<bool>,
    ) -> impl Stream<Item = ImagesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let execute = execute.unwrap_or(false);
        stream! {
            let registry = match ops::registry_for(&config_dir, &org, forge.as_deref()) {
                Ok(r) => r, Err((c, m)) => { yield err(c, m); return; }
            };
            let api = ops::Api::new(&config_dir, registry, &org, endpoint());
            let versions = match api.versions(&name).await {
                Ok(v) => v, Err((c, m)) => { yield err(c, m); return; }
            };
            let plan = match ops::plan_delete(&versions, &name, &tag, with_other_tags.unwrap_or(false)) {
                Ok(p) => p, Err((c, m)) => { yield err(c, m); return; }
            };
            let image = format!("{}/{org}/{name}:{tag}", registry.host());
            if execute {
                if let Err((c, m)) = api.delete(&name, &plan.version, &plan.by).await {
                    yield err(c, m);
                    return;
                }
            }
            yield ImagesEvent::ImageDelete {
                image, digest: plan.version.digest.clone(), other_tags: plan.other_tags,
                deleted: execute, dry_run: !execute,
            };
        }
    }
}
