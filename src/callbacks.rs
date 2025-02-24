//! The main functionality: callbacks for rustc plugin systems.
//! Inspired by <https://github.com/facebookexperimental/MIRAI/blob/9cf3067309d591894e2d0cd9b1ee6e18d0fdd26c/checker/src/callbacks.rs>
extern crate rustc_driver;
extern crate rustc_hir;

use std::path::PathBuf;

use crate::cs;
use crate::options::{CrateNameList, DetectorKind, Options};
use log::{debug, warn};
use rustc_driver::Compilation;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_interface::interface;
use rustc_middle::mir::mono::MonoItem;
use rustc_middle::ty::{Instance, ParamEnv, TyCtxt};

use crate::analysis::callgraph::CallGraph;

use crate::detector::lock::DeadlockDetector;
use crate::detector::lock::Report;

pub struct LockBudCallbacks {
    options: Options,
    file_name: String,
    output_directory: PathBuf,
    test_run: bool,
}

impl LockBudCallbacks {
    pub fn new(options: Options) -> Self {
        Self {
            options,
            file_name: String::new(),
            output_directory: PathBuf::default(),
            test_run: false,
        }
    }
}

impl rustc_driver::Callbacks for LockBudCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        self.file_name = config.input.source_name().prefer_remapped().to_string();
        debug!("Processing input file: {}", self.file_name);
        if config.opts.test {
            debug!("in test only mode");
            // self.options.test_only = true;
        }
        match &config.output_dir {
            None => {
                self.output_directory = std::env::temp_dir();
                self.output_directory.pop();
            }
            Some(path_buf) => self.output_directory.push(path_buf.as_path()),
        }
    }
    fn after_analysis<'tcx>(
        &mut self,
        compiler: &rustc_interface::interface::Compiler,
        queries: &'tcx rustc_interface::Queries<'tcx>,
    ) -> rustc_driver::Compilation {
        compiler.session().abort_if_errors();
        if self
            .output_directory
            .to_str()
            .expect("valid string")
            .contains("/build/")
        {
            // No need to analyze a build script, but do generate code.
            return Compilation::Continue;
        }
        queries
            .global_ctxt()
            .unwrap()
            .peek_mut()
            .enter(|tcx| {
                let reports = self.analyze_with_lockbud(compiler, tcx);
                cs::analyze(tcx, reports).unwrap();
        });
        if self.test_run {
            // We avoid code gen for test cases because LLVM is not used in a thread safe manner.
            Compilation::Stop
        } else {
            // Although LockBud is only a checker, cargo still needs code generation to work.
            Compilation::Continue
        }
    }
}

impl LockBudCallbacks {
    fn analyze_with_lockbud<'tcx>(&mut self, _compiler: &interface::Compiler, tcx: TyCtxt<'tcx>) -> Option<Vec<Report>> {
        // Skip crates by names (white or black list).
        let crate_name = tcx.crate_name(LOCAL_CRATE).to_string();
        match &self.options.crate_name_list {
            CrateNameList::White(crates) if !crates.is_empty() && !crates.contains(&crate_name) => {
                return None
            }
            CrateNameList::Black(crates) if crates.contains(&crate_name) => return None,
            _ => {}
        };
        if tcx.sess.opts.debugging_opts.no_codegen || !tcx.sess.opts.output_types.should_codegen() {
            return None;
        }
        let cgus = tcx.collect_and_partition_mono_items(()).1;
        let instances: Vec<Instance<'tcx>> = cgus
            .iter()
            .flat_map(|cgu| {
                cgu.items().iter().filter_map(|(mono_item, _)| {
                    if let MonoItem::Fn(instance) = mono_item {
                        Some(*instance)
                    } else {
                        None
                    }
                })
            })
            .collect();
        let mut callgraph = CallGraph::new();
        let param_env = ParamEnv::reveal_all();
        callgraph.analyze(instances.clone(), tcx, param_env);
        match self.options.detector_kind {
            DetectorKind::Deadlock => {
                let mut deadlock_detector = DeadlockDetector::new(tcx, param_env);
                let reports = deadlock_detector.detect(&callgraph);
                if !reports.is_empty() {
                    let j = serde_json::to_string_pretty(&reports).unwrap();
                    warn!("{}", j);
                    report_stats(&crate_name, &reports);
                    return Some(reports)
                }
            }
        }

        None
    }
}

fn report_stats(crate_name: &str, reports: &[Report]) {
    let (
        mut doublelock_probably,
        mut doublelock_possibly,
        mut conflictlock_probably,
        mut conflictlock_possibly,
    ) = (0, 0, 0, 0);
    for report in reports {
        match report {
            Report::DoubleLock(doublelock) => match doublelock.possibility.as_str() {
                "Probably" => doublelock_probably += 1,
                "Possibly" => doublelock_possibly += 1,
                _ => {}
            },
            Report::ConflictLock(conflictlock) => match conflictlock.possibility.as_str() {
                "Probably" => conflictlock_probably += 1,
                "Possibly" => conflictlock_possibly += 1,
                _ => {}
            },
        }
    }
    warn!("crate {} contains doublelock: {{ probably: {}, possibly: {} }}, conflictlock: {{ probably: {}, possibly: {} }}", crate_name, doublelock_probably, doublelock_possibly, conflictlock_probably, conflictlock_possibly);
}
