//! One user command coordinates profile validation, project backups and the sandbox lifecycle.
use super::*;
use sandbox_core::models::ComposeLaunchSpec;
use sandbox_projects::launch_profile;
use sandbox_sandboxes::sandbox::Action;

impl Runtime {
    pub fn save_launch_profile(
        &self,
        id: &SandboxId,
        spec: ComposeLaunchSpec,
        cancel: &AtomicBool,
    ) -> Result<ComposeLaunchSpec> {
        ensure!(!self.is_stopping(), "Application is shutting down");
        let manager = self.manager(id)?;
        ensure!(
            !manager.snapshot().busy,
            "Wait for the current sandbox operation"
        );
        let mut sandbox = self.catalog.sandbox(id)?;
        let spec = launch_profile::prepare(&self.catalog, &sandbox, spec, cancel)?;
        let directory = &sandbox
            .source
            .as_ref()
            .context("Choose a working copy first")?
            .directory;
        let portable = launch_profile::portable(spec.clone(), directory);
        sandbox.compose = Some(spec.clone());
        manager.save_configuration(|| self.catalog.save_launch_profile(&sandbox, &portable))?;
        Ok(spec)
    }
    pub fn run_sandbox(self: &Arc<Self>, id: &SandboxId, replace_databases: bool) -> Result<()> {
        ensure!(!self.is_stopping(), "Application is shutting down");
        let manager = self.manager(id)?;
        let runtime = self.clone();
        let id = id.clone();
        let progress = manager.clone();
        manager.launch_prepared(
            if replace_databases {
                Action::Create
            } else {
                Action::Run
            },
            move |controller| {
                controller.reconcile()?;
                controller.report("Validating project launch profile…".into(), None);
                let cancel = controller.cancellation();
                let mut sandbox = runtime.catalog.sandbox(&id)?;
                let spec = sandbox
                    .compose
                    .clone()
                    .or(runtime.catalog.launch_profile(&sandbox.project_id)?)
                    .filter(|spec| !spec.files.is_empty())
                    .context(
                        "Choose Compose files and save the project profile in Projects → Launch",
                    )?;
                let spec = launch_profile::prepare(&runtime.catalog, &sandbox, spec, &cancel)?;
                sandbox.compose = Some(spec);
                runtime.catalog.save_sandbox(&sandbox)?;
                controller.report("Preparing project…".into(), None);
                sandbox_projects::preparation::run(&runtime.catalog, &sandbox, &cancel, |line| {
                    controller.logs.append("Preparation", line)
                })?;
                controller.report("Checking source and configuration mounts…".into(), None);
                sandbox_projects::compose::validate_mounts(&runtime.catalog, &sandbox, &cancel)?;
                ensure!(!cancel.load(Ordering::Acquire), "Cancelled");
                // A normal start never refreshes an existing generation, even if S3 has newer data.
                if replace_databases || !controller.has_database_data()? {
                    let backups = runtime.backups(&sandbox.project_id)?;
                    backups.ensure_latest(&cancel, move |stage, fraction| {
                        progress.report(stage, fraction)
                    })?;
                }
                ensure!(!cancel.load(Ordering::Acquire), "Cancelled");
                Ok(())
            },
        )
    }
}
