//! Layered CI runner: execute build/test commands using [ci] runners in dependency order.

use async_stream::stream;
use futures::Stream;
use std::path::PathBuf;
use std::time::Instant;

use crate::commands::runner::discover_or_bail;
use crate::commands::workspace::build_publish_dep_graph;
use crate::service::HyperforgeEvent;
use crate::services::utils::{dry_prefix, RepoFilter};
use crate::types::config::{resolve_ci_config, RunnerConfig, RunnerType};

/// Run build/test commands using layered CI runners in dependency order.
///
/// - `--level N` runs only the runner at index N
/// - Without `--level`, runs all local-type runners (skips docker)
/// - `--test` also runs the test command after build
pub fn run(
    path: String,
    test: Option<bool>,
    level: Option<usize>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    dry_run: Option<bool>,
    parallel: Option<usize>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let is_dry_run = dry_run.unwrap_or(false);
    let run_tests = test.unwrap_or(false);
    let concurrency = parallel.unwrap_or(0); // 0 = unbounded

    stream! {
        let workspace_path = PathBuf::from(&path);
        let dry = dry_prefix(is_dry_run);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Build dep graph and tiers (excluding dev deps to avoid cycles)
        let graph = build_publish_dep_graph(&ctx.repos);
        let tiers = match graph.build_tiers() {
            Ok(t) => t,
            Err(e) => {
                yield HyperforgeEvent::Error {
                    message: format!("Dependency cycle detected: {e:?}"),
                };
                return;
            }
        };

        // Collect per-repo runner configs
        // Key: repo effective_name → (path, Vec<RunnerConfig>, skip)
        let mut repo_runners: std::collections::HashMap<String, (PathBuf, Vec<RunnerConfig>, bool)> =
            std::collections::HashMap::new();

        for repo in &ctx.repos {
            let name = repo.effective_name();
            if !filter.matches(&repo.dir_name) {
                continue;
            }

            let existing_ci = repo.config.as_ref().and_then(|c| c.ci.as_ref());
            let ci = resolve_ci_config(existing_ci, &repo.build_systems);
            let (runners, skip) = (ci.runners, ci.skip_validate);

            repo_runners.insert(name, (repo.path.clone(), runners, skip));
        }

        // Determine which runner indices to execute per repo
        // --level N: run only runner at index N
        // no --level: run all local-type runners
        let select_runners = |runners: &[RunnerConfig]| -> Vec<usize> {
            if let Some(lvl) = level {
                if lvl < runners.len() {
                    vec![lvl]
                } else {
                    vec![] // level out of range
                }
            } else {
                // All local runners
                runners.iter().enumerate()
                    .filter(|(_, r)| r.runner_type == RunnerType::Local)
                    .map(|(i, _)| i)
                    .collect()
            }
        };

        // Count total work
        let mut total_repos = 0usize;
        let mut total_steps = 0usize;
        let mut skipped = 0usize;
        for (_, runners, skip) in repo_runners.values() {
            if *skip {
                skipped += 1;
                continue;
            }
            let indices = select_runners(runners);
            if !indices.is_empty() {
                total_repos += 1;
                total_steps += indices.len() * if run_tests { 2 } else { 1 };
            }
        }

        let level_desc = match level {
            Some(0) => "level 0 (quick check)",
            Some(1) => "level 1 (full build)",
            Some(2) => "level 2 (containerized)",
            Some(n) => { yield HyperforgeEvent::Info { message: format!("Level {n}") }; "custom" },
            None => "all local runners",
        };

        yield HyperforgeEvent::Info {
            message: format!(
                "{dry}Running {level_desc} — {total_repos} repos, ~{total_steps} steps, {skipped} skipped",
            ),
        };

        let mut passed = 0usize;
        let mut failed = 0usize;
        let overall_start = Instant::now();

        // Process tiers sequentially; within each tier, repos in parallel
        for (tier_idx, tier_nodes) in tiers.iter().enumerate() {
            // Collect repos in this tier that we need to run
            let tier_repos: Vec<_> = tier_nodes.iter().filter_map(|&node_idx| {
                let node = &graph.nodes[node_idx];
                repo_runners.get(&node.name).map(|(path, runners, skip)| {
                    (node.name.clone(), path.clone(), runners.clone(), *skip)
                })
            }).collect();

            if tier_repos.is_empty() {
                continue;
            }

            let tier_active: Vec<_> = tier_repos.iter()
                .filter(|(_, _, _, skip)| !*skip)
                .filter(|(_, _, runners, _)| !select_runners(runners).is_empty())
                .collect();

            if tier_active.is_empty() {
                continue;
            }

            yield HyperforgeEvent::Info {
                message: format!("{}Tier {}: {} repos", dry, tier_idx, tier_active.len()),
            };

            if is_dry_run {
                for (name, _path, runners, skip) in &tier_repos {
                    if *skip {
                        yield HyperforgeEvent::ValidateStep {
                            repo_name: name.clone(),
                            step: "skip".into(),
                            status: "skipped".into(),
                            duration_ms: 0,
                        };
                        continue;
                    }
                    let indices = select_runners(runners);
                    for idx in indices {
                        let runner = &runners[idx];
                        let build_cmd = runner.build.join(" ");
                        yield HyperforgeEvent::ValidateStep {
                            repo_name: name.clone(),
                            step: format!("L{idx} build: {build_cmd}"),
                            status: "dry-run".into(),
                            duration_ms: 0,
                        };
                        if run_tests && !runner.test.is_empty() {
                            let test_cmd = runner.test.join(" ");
                            yield HyperforgeEvent::ValidateStep {
                                repo_name: name.clone(),
                                step: format!("L{idx} test: {test_cmd}"),
                                status: "dry-run".into(),
                                duration_ms: 0,
                            };
                        }
                    }
                }
                continue;
            }

            // Execute tier repos in parallel via run_batch
            let batch_inputs: Vec<_> = tier_active.iter().map(|(name, path, runners, _)| {
                let indices = select_runners(runners);
                (name.clone(), path.clone(), runners.clone(), indices)
            }).collect();

            let run_tests_copy = run_tests;
            let results = crate::commands::runner::run_batch(
                batch_inputs,
                concurrency,
                move |(repo_name, repo_path, runners, indices)| async move {
                    let run_tests = run_tests_copy;
                    let mut steps = Vec::new();
                    for idx in indices {
                        let runner = &runners[idx];
                        // Build step
                        if !runner.build.is_empty() {
                            let start = Instant::now();
                            let result = execute_runner_cmd(
                                &runner.build,
                                &repo_path,
                                &runner.env,
                                runner.timeout_secs,
                                &runner.runner_type,
                                runner.image.as_deref(),
                            ).await;
                            let duration_ms = start.elapsed().as_millis() as u64;
                            let (status, ok) = match result {
                                Ok(output) if output.success => ("passed".into(), true),
                                Ok(output) => (format!("failed (exit {}): {}", output.code, output.stderr.chars().take(200).collect::<String>()), false),
                                Err(e) => (format!("error: {e}"), false),
                            };
                            steps.push((
                                repo_name.clone(),
                                format!("L{idx} build"),
                                status,
                                duration_ms,
                                ok,
                            ));
                            if !ok {
                                break; // Don't continue to test if build failed
                            }
                        }

                        // Test step
                        if run_tests && !runner.test.is_empty() {
                            let start = Instant::now();
                            let result = execute_runner_cmd(
                                &runner.test,
                                &repo_path,
                                &runner.env,
                                runner.timeout_secs,
                                &runner.runner_type,
                                runner.image.as_deref(),
                            ).await;
                            let duration_ms = start.elapsed().as_millis() as u64;
                            let (status, ok) = match result {
                                Ok(output) if output.success => ("passed".into(), true),
                                Ok(output) => (format!("failed (exit {}): {}", output.code, output.stderr.chars().take(200).collect::<String>()), false),
                                Err(e) => (format!("error: {e}"), false),
                            };
                            steps.push((
                                repo_name.clone(),
                                format!("L{idx} test"),
                                status,
                                duration_ms,
                                ok,
                            ));
                            if !ok {
                                break;
                            }
                        }
                    }
                    steps
                },
            ).await;

            for result in results {
                match result {
                    Ok(steps) => {
                        for (repo_name, step, status, duration_ms, ok) in steps {
                            yield HyperforgeEvent::ValidateStep {
                                repo_name,
                                step,
                                status,
                                duration_ms,
                            };
                            if ok { passed += 1; } else { failed += 1; }
                        }
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Task error: {e}"),
                        };
                        failed += 1;
                    }
                }
            }

            // If any failures in this tier, stop (deps failed, downstream won't work)
            if failed > 0 {
                yield HyperforgeEvent::Error {
                    message: format!("Tier {tier_idx} had failures — stopping"),
                };
                break;
            }
        }

        let total_ms = overall_start.elapsed().as_millis() as u64;
        yield HyperforgeEvent::ValidateSummary {
            total: passed + failed + skipped,
            passed,
            failed,
            skipped,
            duration_ms: total_ms,
        };
    }
}

struct CmdOutput {
    success: bool,
    code: i32,
    #[allow(dead_code)]
    stdout: String,
    stderr: String,
}

/// Initialize CI configs for all workspace repos that lack a [ci] section.
///
/// For each repo with a detected build system but no CI config:
/// - Generates default layered runners via `resolve_ci_config`
/// - Writes the [ci] section to `.hyperforge/config.toml`
///
/// Repos that already have a [ci] section are left untouched.
/// Repos with no detected build system get `skip_validate: true`.
pub fn init_configs(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let is_dry_run = dry_run.unwrap_or(false);

    stream! {
        let workspace_path = PathBuf::from(&path);
        let dry = dry_prefix(is_dry_run);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        let mut written = 0usize;
        let mut skipped_existing = 0usize;
        let mut skipped_no_bs = 0usize;

        for repo in &ctx.repos {
            if !filter.matches(&repo.dir_name) {
                continue;
            }

            // Resolve CI config — returns existing if present, defaults otherwise
            let existing_ci = repo.config.as_ref().and_then(|c| c.ci.as_ref());
            let ci = resolve_ci_config(existing_ci, &repo.build_systems);

            // Skip if repo already has CI config
            if existing_ci.is_some() {
                skipped_existing += 1;
                continue;
            }

            // Skip repos with no real build system (they get skip_validate: true)
            if ci.skip_validate && ci.runners.is_empty() {
                skipped_no_bs += 1;
                continue;
            }

            let name = repo.effective_name();
            let runner_desc: Vec<String> = ci.runners.iter().enumerate().map(|(i, r)| {
                let ty = match r.runner_type {
                    RunnerType::Local => "local",
                    RunnerType::Docker => "docker",
                };
                format!("L{}: {} {}", i, ty, r.build.join(" "))
            }).collect();

            yield HyperforgeEvent::Info {
                message: format!("{}{}: {} runners [{}]", dry, name, ci.runners.len(), runner_desc.join(", ")),
            };

            if is_dry_run {
                written += 1;
            } else {
                // Load existing config or create minimal one
                let mut config = repo.config.clone().unwrap_or_else(|| {
                    crate::config::HyperforgeConfig::default()
                });
                config.ci = Some(ci);

                match config.save(&repo.path) {
                    Ok(()) => { written += 1; }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to write config for {name}: {e}"),
                        };
                    }
                }
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{dry}CI init: {written} written, {skipped_existing} already configured, {skipped_no_bs} no build system",
            ),
        };
    }
}

async fn execute_runner_cmd(
    cmd: &[String],
    working_dir: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    timeout_secs: u64,
    runner_type: &RunnerType,
    image: Option<&str>,
) -> Result<CmdOutput, String> {
    match runner_type {
        RunnerType::Local => {
            let shell_cmd = cmd.join(" ");
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(&shell_cmd).current_dir(working_dir);
            for (k, v) in env {
                command.env(k, v);
            }

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                command.output(),
            ).await;

            match result {
                Ok(Ok(output)) => Ok(CmdOutput {
                    success: output.status.success(),
                    code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                }),
                Ok(Err(e)) => Err(format!("Failed to execute: {e}")),
                Err(_) => Err(format!("Timeout after {timeout_secs}s")),
            }
        }
        RunnerType::Docker => {
            let img = image.unwrap_or("rust:latest");
            let shell_cmd = cmd.join(" ");
            let work_dir = working_dir.display().to_string();

            let mut docker_args = vec![
                "run".to_string(),
                "--rm".to_string(),
                "-v".to_string(),
                format!("{}:/workspace", work_dir),
                "-w".to_string(),
                "/workspace".to_string(),
            ];
            for (k, v) in env {
                docker_args.push("-e".to_string());
                docker_args.push(format!("{k}={v}"));
            }
            docker_args.push(img.to_string());
            docker_args.push("sh".to_string());
            docker_args.push("-c".to_string());
            docker_args.push(shell_cmd);

            let mut command = tokio::process::Command::new("docker");
            command.args(&docker_args);

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                command.output(),
            ).await;

            match result {
                Ok(Ok(output)) => Ok(CmdOutput {
                    success: output.status.success(),
                    code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                }),
                Ok(Err(e)) => Err(format!("Failed to execute docker: {e}")),
                Err(_) => Err(format!("Docker timeout after {timeout_secs}s")),
            }
        }
    }
}
