use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "homebrew",
    category: super::Category::Host,
    description: "Discovered Homebrew installations and linked libraries",
    probe: None,
    markers: &[],
    detect: |host| !host.homebrew.is_empty(),
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, _context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    for prefix in &host.homebrew {
        for child in ["Cellar", "opt", "lib", "share"] {
            plan.read_existing(prefix.join(child));
        }
        for child in ["bin", "sbin"] {
            plan.bin(prefix.join(child));
        }
        if let Ok(formulas) = std::fs::read_dir(prefix.join("opt")) {
            for formula in formulas.flatten() {
                let name = formula.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name == "openssl"
                    || name.strip_prefix("openssl@").is_some_and(|version| {
                        !version.is_empty()
                            && version.bytes().all(|ch| ch.is_ascii_digit() || ch == b'.')
                    })
                {
                    for file in ["openssl.cnf", "cert.pem"] {
                        plan.read_existing(prefix.join("etc").join(name).join(file));
                    }
                }
            }
        }
    }
    Ok(plan)
}
