use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use axoproject::WorkspaceSearch;
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use miette::Report;

use crate::commands::{Build, Serve};
use oranda::data::workspaces;
use oranda::data::workspaces::WorkspaceData;
use oranda::site::Site;
use oranda::{
    config::Config,
    errors::*,
    site::mdbook::{custom_theme, load_mdbook, mdbook_dir},
};

#[derive(Clone, Debug, Parser)]
pub struct Dev {
    /// The port for the file server to be launched on
    #[arg(long)]
    port: Option<u16>,
    /// DO NOT USE: Path to the root dir of the project
    ///
    /// This flag exists for internal testing. It is incorrectly implemented for actual
    /// end-users and will make you very confused and sad.
    #[clap(hide = true)]
    #[arg(long)]
    project_root: Option<Utf8PathBuf>,
    /// DO NOT USE: Path to the oranda.json
    ///
    /// This flag exists for internal testing. It is incorrectly implemented for actual
    /// end-users and will make you very confused and sad.
    #[clap(hide = true)]
    #[arg(long)]
    config_path: Option<Utf8PathBuf>,
    /// Skip the first build before starting to watch for changes
    #[arg(long)]
    no_first_build: bool,
    /// List of extra paths to watch
    #[arg(short, long)]
    include_paths: Option<Vec<Utf8PathBuf>>,
}

impl Dev {
    pub fn run(self) -> Result<()> {
        let root_path = Utf8PathBuf::from_path_buf(std::env::current_dir()?.canonicalize()?)
            .unwrap_or(Utf8PathBuf::new());
        let mut paths_to_watch = if let Ok(Some(config)) = Site::get_workspace_config() {
            let mut workspace_config_path = root_path.clone();
            workspace_config_path.push("oranda-workspace.json");
            let members = workspaces::from_config(&config, &root_path, &workspace_config_path)?;
            let mut ret = Vec::new();
            for member in members {
                let mut paths =
                    self.collect_paths_for_site(&member.config, &root_path, Some(member.clone()))?;
                ret.append(&mut paths);
            }
            // Also watch oranda-workspace.json
            ret.push(Utf8PathBuf::from("oranda-workspace.json"));
            ret
        } else {
            let config = Config::build(
                &self
                    .config_path
                    .clone()
                    .unwrap_or(Utf8PathBuf::from("./oranda.json")),
            )?;
            self.collect_paths_for_site(&config, &root_path, None)?
        };

        // Watch for any user-provided paths
        if let Some(include_paths) = &self.include_paths {
            let mut include_paths = include_paths.clone();
            paths_to_watch.append(&mut include_paths);
        }

        let (tx, rx) = std::sync::mpsc::channel();

        // We debounce events so that we don't end up building 5 times in a row because 5 different
        // filesystem events fired.
        let mut debouncer = notify_debouncer_mini::new_debouncer(Duration::from_secs(1), None, tx)?;
        let watcher = debouncer.watcher();
        let mut existing_paths = vec![];
        for path in paths_to_watch {
            let path = PathBuf::from(path);
            // If no path exists, oranda won't work anyways, so there's not much need to let the user know
            // (they'll know either way ;) )
            if path.exists() {
                watcher.watch(
                    path.as_path(),
                    notify_debouncer_mini::notify::RecursiveMode::Recursive,
                )?;
                existing_paths.push(path.clone());
            }
        }

        tracing::info!(
            "Found {} paths to watch, starting watch...",
            existing_paths.len()
        );
        tracing::debug!("Files watched: {:?}", existing_paths);

        if !self.no_first_build {
            Build::new(self.project_root.clone(), self.config_path.clone()).run()?;
        }

        // Spawn the serve process out into a separate thread so that we can loop through received events on this thread
        let _ = std::thread::spawn(move || Serve::new(self.port).run());
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port.unwrap_or(7979)));
        let msg = format!("Your project is available at: http://{}", addr);
        tracing::info!(success = true, "{}", &msg);
        loop {
            // Wait for all debounced events to arrive
            let first_event = rx.recv().expect("channel shut down incorrectly");
            sleep(Duration::from_millis(50));
            let other_events = rx.try_iter();

            let all_events = std::iter::once(first_event).chain(other_events);
            // Unpack events into paths
            let paths: Vec<_> = all_events
                .filter_map(|event| match event {
                    Ok(events) => Some(events),
                    Err(errors) => {
                        for error in errors {
                            tracing::warn!("Error while watching for changes: {error}",);
                        }
                        None
                    }
                })
                .flatten()
                .map(|event| event.path)
                .collect();

            if !paths.is_empty() {
                tracing::info!("Path(s) {:?} changed, rebuilding...", paths);

                if let Err(e) =
                    Build::new(self.project_root.clone(), self.config_path.clone()).run()
                {
                    eprintln!("{:?}", Report::new(e));
                    continue;
                }
            }
        }
    }

    fn collect_paths_for_site(
        &self,
        config: &Config,
        root_path: &Utf8PathBuf,
        workspace: Option<WorkspaceData>,
    ) -> Result<Vec<Utf8PathBuf>> {
        let config = config.clone();
        let member_path = workspace.map(|w| w.path);
        let mut paths_to_watch = vec![];

        // Watch for the readme file
        paths_to_watch.push(pathdiff(
            root_path,
            &member_path,
            config.project.readme_path,
        )?);

        // Watch for the oranda config file
        let cfg_file = self
            .config_path
            .clone()
            .unwrap_or(Utf8PathBuf::from("./oranda.json"));
        paths_to_watch.push(pathdiff(root_path, &member_path, cfg_file)?);

        // Watch for the funding.md page and the funding.yml file
        if let Some(funding) = &config.components.funding {
            if let Some(path) = &funding.yml_path {
                paths_to_watch.push(pathdiff(root_path, &member_path, path)?);
            }
            if let Some(path) = &funding.md_path {
                paths_to_watch.push(pathdiff(root_path, &member_path, path)?);
            }
        }

        // Watch for additional pages, if we have any
        if !config.build.additional_pages.is_empty() {
            let mut additional_pages = config
                .build
                .additional_pages
                .values()
                .cloned()
                .map(|p| pathdiff(root_path, &member_path, p).unwrap())
                .collect();
            paths_to_watch.append(&mut additional_pages);
        }

        // Watch for the mdbook directory, if we have it
        if let Some(book_cfg) = &config.components.mdbook {
            let path = mdbook_dir(book_cfg)?;
            let md = load_mdbook(&path)?;
            // watch book.toml and /src/
            let book_path = pathdiff(
                root_path,
                &member_path,
                md.root.join("book.toml").display().to_string(),
            )?;
            let source_path = pathdiff(
                root_path,
                &member_path,
                md.source_dir().display().to_string(),
            )?;
            paths_to_watch.push(book_path);
            paths_to_watch.push(source_path);

            // If we're not clobbering the theme, also watch the theme dir
            // (note that this may not exist on the fs, mdbook reports the path regardless)
            if custom_theme(book_cfg, &config.styles.theme).is_none() {
                let theme_path = pathdiff(
                    root_path,
                    &member_path,
                    md.theme_dir().display().to_string(),
                )?;
                paths_to_watch.push(theme_path);
            }
        }

        // Watch for any project manifest files
        let project = axoproject::get_workspaces("./".into(), None);
        if let WorkspaceSearch::Found(workspace) = project.rust {
            paths_to_watch.push(pathdiff(root_path, &member_path, workspace.manifest_path)?);
        }
        if let WorkspaceSearch::Found(workspace) = project.javascript {
            paths_to_watch.push(pathdiff(root_path, &member_path, workspace.manifest_path)?);
        }
        Ok(paths_to_watch)
    }
}

/// Creates a workspace-safe relative path. Takes the following arguments:
/// - The root path of the workspace (or single project)
/// - An optional workspace member path
/// - The path itself, usually extracted from the configuration
/// Member path and the path itself can be relative or absolute .
/// The function will attempt to lazily build the smallest possible absolute and canonicalized path,
/// before diffing it with the root path to create a path that's always relative to the workspace root.
///
/// Some example scenarios:
/// 1. root path = "/my/directory", member path = None, path = "myfile.md"
///    Output = "myfile.md"
/// 2. root path = "/my/directory", member path = "member", path = "myfile.md"
///    Output = "member/myfile.md"
/// 3. root path= "/my/directory", member path = "/my/directory/member", path = "../root.md"
///    Output = "root.md"
fn pathdiff(
    root_path: impl AsRef<Utf8Path>,
    member_path: &Option<impl AsRef<Utf8Path>>,
    path: impl AsRef<Utf8Path>,
) -> Result<Utf8PathBuf> {
    let root_path = root_path.as_ref();
    let member_path = member_path.as_ref().map(|p| p.as_ref());
    let path = path.as_ref();
    if path.is_absolute() {
        // If absolute, return the path
        return Ok(path.to_owned());
    }

    // If the member path exists and is absolute, construct `member_path/path`.
    // If the member path exists and isn't absolute, construct `root_path/member_path/path`.
    // If the member path doesn't exist, construct `root_path/path`.
    let path_plus_member = if let Some(member_path) = member_path {
        if member_path.is_absolute() {
            let mut owned = Utf8PathBuf::new();
            owned.push(member_path);
            owned.push(path);
            owned.canonicalize_utf8()
        } else {
            let mut owned = Utf8PathBuf::new();
            owned.push(root_path);
            owned.push(member_path);
            owned.push(path);
            owned.canonicalize_utf8()
        }
    } else {
        let mut owned = Utf8PathBuf::new();
        owned.push(root_path);
        owned.push(path);
        owned.canonicalize_utf8()
    };

    match path_plus_member {
        Ok(path) => {
            // Create a relative path from difference between root and created path.
            Ok(
                pathdiff::diff_utf8_paths(&path, root_path).ok_or(OrandaError::PathdiffError {
                    root_path: root_path.to_string(),
                    path: path.to_string(),
                })?,
            )
        }
        Err(_) => {
            // The path probably doesn't exist, return an empty path
            return Ok(Utf8PathBuf::new());
        }
    }
}
