use anyhow::{Context, Result};

pub(crate) fn configure_current(requested: Option<&str>) -> Result<()> {
    let launch = std::env::current_dir().context("could not determine the working directory")?;
    let root = crate::workspace::discover_project_root(&launch)?;
    let loaded = match requested {
        Some(raw) => {
            crate::workspace::load_file(&crate::workspace::absolute(raw, &launch)?, &root)?
        }
        None => crate::workspace::load(&root)?,
    };
    crate::policy::validate_project(&root, &loaded)?;
    println!(
        "Project: {}\nPolicy: {}\n{}",
        root.display(),
        loaded.source.display(),
        serde_json::to_string_pretty(&loaded.config)?
    );
    Ok(())
}
