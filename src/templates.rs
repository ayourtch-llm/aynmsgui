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
    pub login: mustache::Template,
    pub operations: mustache::Template,
    pub settings_credentials: mustache::Template,
    pub assignments: mustache::Template,
    /// Generic title + message page (used for errors, conflicts, simple results).
    pub message: mustache::Template,
}

impl Templates {
    /// Load and compile all templates from the given directory.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        info!(dir = %dir.display(), "Loading mustache templates");
        Ok(Self {
            base: compile(&dir.join("base.html.mustache"))?,
            dashboard: compile(&dir.join("dashboard.html.mustache"))?,
            login: compile(&dir.join("login.html.mustache"))?,
            operations: compile(&dir.join("operations.html.mustache"))?,
            settings_credentials: compile(&dir.join("settings_credentials.html.mustache"))?,
            assignments: compile(&dir.join("assignments.html.mustache"))?,
            message: compile(&dir.join("message.html.mustache"))?,
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

    /// Convenience: render the generic message page (title + body + back link)
    /// wrapped in the base layout. `message` is plain text (auto-escaped);
    /// `message_html` is raw HTML (use with care).
    pub fn render_message(
        &self,
        title: &str,
        message: Option<&str>,
        message_html: Option<&str>,
        back: Option<(&str, &str)>,
    ) -> anyhow::Result<String> {
        #[derive(Serialize)]
        struct MsgCtx<'a> {
            title: &'a str,
            message: Option<&'a str>,
            message_html: Option<&'a str>,
            back_href: Option<&'a str>,
            back_label: &'a str,
        }
        let ctx = MsgCtx {
            title,
            message,
            message_html,
            back_href: back.map(|(h, _)| h),
            back_label: back.map(|(_, l)| l).unwrap_or(""),
        };
        self.render_page(&self.message, title, "", &ctx)
    }

    /// Render a single template directly (no base layout wrapping).
    /// Use for pages that should not show the nav (e.g. login).
    pub fn render_standalone<T: Serialize>(
        &self,
        template: &mustache::Template,
        data: &T,
    ) -> anyhow::Result<String> {
        let mut out = Vec::new();
        template
            .render(&mut out, data)
            .map_err(|e| anyhow::anyhow!("failed to render template: {e:?}"))?;
        String::from_utf8(out).context("template rendered non-utf8 output")
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
