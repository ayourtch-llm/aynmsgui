//! Compiled-template registry for HTML rendering.
//!
//! Templates live under `templates/` (relative to cwd) and are compiled once
//! at startup. The `base.html.mustache` layout takes `{title, user_display,
//! content_html}` and embeds the page-specific body via `{{{content_html}}}`
//! (raw / unescaped — pages are responsible for their own escaping, which
//! mustache does for them by default).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use tracing::info;

pub struct Templates {
    pub base: mustache::Template,
    pub dashboard: mustache::Template,
}

impl Templates {
    /// Load and compile all templates from the given directory.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        info!(dir = %dir.display(), "Loading mustache templates");
        Ok(Self {
            base: compile(&dir.join("base.html.mustache"))?,
            dashboard: compile(&dir.join("dashboard.html.mustache"))?,
        })
    }

    /// Load from the default location (`templates/` relative to cwd, then
    /// falling back to `$CARGO_MANIFEST_DIR/templates` for tests).
    pub fn load_default() -> anyhow::Result<Self> {
        let cwd_path = PathBuf::from("templates");
        if cwd_path.join("base.html.mustache").exists() {
            return Self::load(&cwd_path);
        }
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates");
        Self::load(&manifest)
    }

    /// Render a page template, then wrap it in the base layout.
    /// `username` may be empty — falls back to "user" in the header.
    pub fn render_page<T: Serialize>(
        &self,
        page: &mustache::Template,
        title: &str,
        username: &str,
        data: &T,
    ) -> anyhow::Result<String> {
        let mut inner = Vec::new();
        page.render(&mut inner, data)
            .map_err(|e| anyhow::anyhow!("failed to render page template: {e:?}"))?;
        let content_html = String::from_utf8(inner).context("page rendered non-utf8 output")?;

        #[derive(Serialize)]
        struct BaseCtx<'a> {
            title: &'a str,
            user_display: &'a str,
            content_html: &'a str,
        }
        let user_display = if username.is_empty() { "user" } else { username };
        let mut out = Vec::new();
        self.base
            .render(
                &mut out,
                &BaseCtx {
                    title,
                    user_display,
                    content_html: &content_html,
                },
            )
            .map_err(|e| anyhow::anyhow!("failed to render base layout: {e:?}"))?;
        String::from_utf8(out).context("base rendered non-utf8 output")
    }
}

fn compile(path: &Path) -> anyhow::Result<mustache::Template> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading template {}", path.display()))?;
    mustache::compile_str(&content)
        .map_err(|e| anyhow::anyhow!("compiling template {}: {e:?}", path.display()))
}
