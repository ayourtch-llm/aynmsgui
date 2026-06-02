//! Compiled-template registry for HTML rendering.
//!
//! Templates live under `templates/` (relative to cwd) and are compiled once
//! at startup. `base.mustache` is the site-wide layout (`{title, user_display,
//! content_html}`); per-page templates render their body which is then embedded
//! via `{{{content_html}}}` (raw — pages auto-escape their own `{{var}}`
//! interpolations).
//!
//! Templates can include partials via `{{> name}}`, which resolves to
//! `name.mustache` in the same directory.

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
    pub diff_overview: mustache::Template,
    pub diff_detail: mustache::Template,
    pub import_form: mustache::Template,
    pub import_result: mustache::Template,
    pub extract_form: mustache::Template,
    pub extract_sw_form: mustache::Template,
    pub software: mustache::Template,
    pub upgrade_started: mustache::Template,
    pub retrieve_form: mustache::Template,
    pub retrieve_result: mustache::Template,
    pub apply_confirm: mustache::Template,
    pub apply_result: mustache::Template,
    pub assets_list: mustache::Template,
    pub asset_detail: mustache::Template,
    pub devices_list: mustache::Template,
    pub device_detail: mustache::Template,
    /// Generic title + message page (used for errors, conflicts, simple results).
    pub message: mustache::Template,
}

impl Templates {
    /// Load and compile all templates from the given directory.
    /// Uses `mustache::compile_path` so `{{> name}}` partials resolve from
    /// the same directory (looking for `name.mustache`).
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        info!(dir = %dir.display(), "Loading mustache templates");
        let load = |name: &str| compile(&dir.join(format!("{name}.mustache")));
        Ok(Self {
            base: load("base")?,
            dashboard: load("dashboard")?,
            login: load("login")?,
            operations: load("operations")?,
            settings_credentials: load("settings_credentials")?,
            assignments: load("assignments")?,
            diff_overview: load("diff_overview")?,
            diff_detail: load("diff_detail")?,
            import_form: load("import_form")?,
            import_result: load("import_result")?,
            extract_form: load("extract_form")?,
            extract_sw_form: load("extract_sw_form")?,
            software: load("software")?,
            upgrade_started: load("upgrade_started")?,
            retrieve_form: load("retrieve_form")?,
            retrieve_result: load("retrieve_result")?,
            apply_confirm: load("apply_confirm")?,
            apply_result: load("apply_result")?,
            assets_list: load("assets_list")?,
            asset_detail: load("asset_detail")?,
            devices_list: load("devices_list")?,
            device_detail: load("device_detail")?,
            message: load("message")?,
        })
    }

    /// Load from the default location (`templates/` relative to cwd, then
    /// falling back to `$CARGO_MANIFEST_DIR/templates` for tests).
    pub fn load_default() -> anyhow::Result<Self> {
        let cwd_path = PathBuf::from("templates");
        if cwd_path.join("base.mustache").exists() {
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

/// Compile a template from a path. Uses `mustache::compile_path` so that
/// partial references (`{{> name}}`) resolve to `name.mustache` in the
/// same directory.
fn compile(path: &Path) -> anyhow::Result<mustache::Template> {
    mustache::compile_path(path)
        .map_err(|e| anyhow::anyhow!("compiling template {}: {e:?}", path.display()))
}
