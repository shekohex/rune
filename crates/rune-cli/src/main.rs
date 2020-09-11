//! <div align="center">
//!     <img alt="Rune Logo" src="https://raw.githubusercontent.com/rune-rs/rune/master/assets/icon.png" />
//! </div>
//!
//! <br>
//!
//! <div align="center">
//! <a href="https://rune-rs.github.io/rune/">
//!     <b>Read the Book 📖</b>
//! </a>
//! </div>
//!
//! <br>
//!
//! <div align="center">
//! <a href="https://github.com/rune-rs/rune/actions">
//!     <img alt="Build Status" src="https://github.com/rune-rs/rune/workflows/Build/badge.svg">
//! </a>
//!
//! <a href="https://github.com/rune-rs/rune/actions">
//!     <img alt="Book Status" src="https://github.com/rune-rs/rune/workflows/Book/badge.svg">
//! </a>
//!
//! <a href="https://crates.io/crates/rune">
//!     <img alt="crates.io" src="https://img.shields.io/crates/v/rune.svg">
//! </a>
//!
//! <a href="https://docs.rs/rune">
//!     <img alt="docs.rs" src="https://docs.rs/rune/badge.svg">
//! </a>
//!
//! <a href="https://discord.gg/v5AeNkT">
//!     <img alt="Chat on Discord" src="https://img.shields.io/discord/558644981137670144.svg?logo=discord&style=flat-square">
//! </a>
//! </div>
//!
//! A cli for the [Rune Language].
//!
//! If you're in the repo, you can take it for a spin with:
//!
//! ```text
//! cargo run -- scripts/hello_world.rn
//! ```
//!
//! [Rune Language]: https://github.com/rune-rs/rune
//! [runestick]: https://github.com/rune-rs/rune

use anyhow::Result;
use argh::FromArgs;
use rune::termcolor::{ColorChoice, StandardStream};
use rune::EmitDiagnostics as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use runestick::{Item, Unit, Value, VmExecution};

/// Rune Programming Language.
/// CLI Arguments
#[derive(Debug, Clone, FromArgs)]
struct Args {
    /// rune Entry File
    #[argh(positional)]
    path: PathBuf,
    /// provide detailed tracing for each instruction executed.
    #[argh(switch)]
    trace: bool,
    /// dump everything.
    #[argh(switch, short = 'd')]
    dump: bool,
    /// dump default information about unit.
    #[argh(switch)]
    dump_unit: bool,
    /// dump unit instructions.
    #[argh(switch)]
    dump_instructions: bool,
    /// dump the state of the stack after completion. If compiled with `--trace` will dump it after each instruction.
    #[argh(switch)]
    dump_stack: bool,
    /// dump dynamic functions.
    #[argh(switch)]
    dump_functions: bool,
    /// dump dynamic types.
    #[argh(switch)]
    dump_types: bool,
    /// dump native functions.
    #[argh(switch)]
    dump_native_functions: bool,
    /// dump native types.
    #[argh(switch)]
    dump_native_types: bool,
    /// include source code references where appropriate (only available if -O debug-info=true).
    #[argh(switch)]
    with_source: bool,
    /// enabled experimental features.
    #[argh(switch)]
    experimental: bool,
    /// update the given compiler option (seprated by ",").
    /// link-checks: Perform link-time checks,
    /// memoize_instance_fn: Memoize the instance function in a loop,
    /// debug_info: Include debug information when compiling,
    /// macros: Support (experimental) macros,
    /// bytecode: Support (experimental) bytecode caching,
    #[argh(option, short = 'O')]
    compiler_options: rune::Options,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args: Args = argh::from_env();
    let mut context = rune::default_context()?;
    let options = args.compiler_options;

    if args.experimental {
        context.install(&rune_macros::module()?)?;
    }

    let context = Arc::new(context);
    let mut sources = rune::Sources::new();
    let mut warnings = rune::Warnings::new();

    let unit = get_or_build_unit(
        &args,
        (options, context.clone(), &mut sources, &mut warnings),
    )?;

    if !warnings.is_empty() {
        let mut writer = StandardStream::stderr(ColorChoice::Always);
        warnings.emit_diagnostics(&mut writer, &sources)?;
    }

    let vm = runestick::Vm::new(context.clone(), unit.clone());

    if args.dump_native_functions || args.dump {
        println!("# functions");

        for (i, (hash, f)) in context.iter_functions().enumerate() {
            println!("{:04} = {} ({})", i, f, hash);
        }
    }

    if args.dump_native_types || args.dump {
        println!("# types");

        for (i, (hash, ty)) in context.iter_types().enumerate() {
            println!("{:04} = {} ({})", i, ty, hash);
        }
    }

    if args.dump_unit || args.dump {
        dump_unit(&args, &vm, &sources)?;
    }

    let last = std::time::Instant::now();

    let mut execution: runestick::VmExecution = vm.call(&Item::of(&["main"]), ())?;

    let result = if args.trace {
        match do_trace(
            &mut execution,
            &sources,
            args.dump_stack || args.dump,
            args.with_source,
        )
        .await
        {
            Ok(value) => Ok(value),
            Err(TraceError::Io(io)) => return Err(io.into()),
            Err(TraceError::VmError(vm)) => Err(vm),
        }
    } else {
        execution.async_complete().await
    };

    let errored = match result {
        Ok(result) => {
            let duration = std::time::Instant::now().duration_since(last);
            println!("== {:?} ({:?})", result, duration);
            None
        }
        Err(error) => {
            let duration = std::time::Instant::now().duration_since(last);
            println!("== ! ({}) ({:?})", error, duration);
            Some(error)
        }
    };

    if args.dump_stack || args.dump {
        dump_stack(&execution)?;
    }

    if let Some(error) = errored {
        let mut writer = StandardStream::stderr(ColorChoice::Always);
        error.emit_diagnostics(&mut writer, &sources)?;
    }

    Ok(())
}

fn get_or_build_unit(
    args: &Args,
    (options, context, sources, warnings): (
        rune::Options,
        Arc<runestick::Context>,
        &mut rune::Sources,
        &mut rune::Warnings,
    ),
) -> Result<Arc<Unit>> {
    let bytecode_path = args.path.with_extension("rnc");
    let use_cache = options.bytecode && should_cache_be_used(&args.path, &bytecode_path)?;
    let maybe_unit = if use_cache {
        let f = fs::File::open(&bytecode_path)?;
        match bincode::deserialize_from::<_, Unit>(f) {
            Ok(unit) => {
                log::trace!("using cache: {}", bytecode_path.display());
                Some(Arc::new(unit))
            }
            Err(e) => {
                log::error!("failed to deserialize: {}: {}", bytecode_path.display(), e);
                None
            }
        }
    } else {
        None
    };

    let unit = match maybe_unit {
        Some(unit) => unit,
        None => {
            log::trace!("building file: {}", args.path.display());

            let unit = match rune::load_path(&*context, &options, sources, &args.path, warnings) {
                Ok(unit) => unit,
                Err(error) => {
                    let mut writer = StandardStream::stderr(ColorChoice::Always);
                    error.emit_diagnostics(&mut writer, sources)?;
                    anyhow::bail!("Load Error");
                }
            };

            if options.bytecode {
                log::trace!("serializing cache: {}", bytecode_path.display());
                let f = fs::File::create(&bytecode_path)?;
                bincode::serialize_into(f, &unit)?;
            }

            Arc::new(unit)
        }
    };
    Ok(unit)
}

fn dump_stack(execution: &runestick::VmExecution) -> Result<()> {
    println!("# full stack dump after halting");

    let vm = execution.vm()?;

    let frames = vm.call_frames();
    let stack = vm.stack();

    let mut it = frames.iter().enumerate().peekable();

    while let Some((count, frame)) = it.next() {
        let stack_top = match it.peek() {
            Some((_, next)) => next.stack_bottom(),
            None => stack.stack_bottom(),
        };

        let values = stack
            .get(frame.stack_bottom()..stack_top)
            .expect("bad stack slice");

        println!("  frame #{} (+{})", count, frame.stack_bottom());

        if values.is_empty() {
            println!("    *empty*");
        }

        for (n, value) in stack.iter().enumerate() {
            println!("{}+{} = {:?}", frame.stack_bottom(), n, value);
        }
    }

    // NB: print final frame
    println!("  frame #{} (+{})", frames.len(), stack.stack_bottom());

    let values = stack.get(stack.stack_bottom()..).expect("bad stack slice");

    if values.is_empty() {
        println!("    *empty*");
    }

    for (n, value) in values.iter().enumerate() {
        println!("    {}+{} = {:?}", stack.stack_bottom(), n, value);
    }
    Ok(())
}

fn dump_unit(args: &Args, vm: &runestick::Vm, sources: &rune::Sources) -> Result<()> {
    use std::io::Write as _;

    let unit = vm.unit();

    if args.dump_instructions || args.dump {
        println!("# instructions");

        let mut first_function = true;

        for (n, inst) in unit.iter_instructions().enumerate() {
            let out = std::io::stdout();
            let mut out = out.lock();

            let debug = unit.debug_info().and_then(|d| d.instruction_at(n));

            if let Some((hash, signature)) = unit.debug_info().and_then(|d| d.function_at(n)) {
                if first_function {
                    first_function = false;
                } else {
                    println!();
                }

                println!("fn {} ({}):", signature, hash);
            }

            if args.with_source {
                if let Some((source, span)) =
                    debug.and_then(|d| sources.get(d.source_id).map(|s| (s, d.span)))
                {
                    if let Some((count, line)) = rune::diagnostics::line_for(source.as_str(), span)
                    {
                        writeln!(
                            out,
                            "  {}:{: <3} - {}",
                            source.name(),
                            count + 1,
                            line.trim_end()
                        )?;
                    }
                }
            }

            if let Some(label) = debug.and_then(|d| d.label.as_ref()) {
                println!("{}:", label);
            }

            write!(out, "  {:04} = {}", n, inst)?;

            if let Some(comment) = debug.and_then(|d| d.comment.as_ref()) {
                write!(out, " // {}", comment)?;
            }

            println!();
        }
    }

    let mut functions = unit.iter_functions().peekable();
    let mut types = unit.iter_types().peekable();
    let mut strings = unit.iter_static_strings().peekable();
    let mut keys = unit.iter_static_object_keys().peekable();

    if (args.dump_functions || args.dump) && functions.peek().is_some() {
        println!("# dynamic functions");

        for (hash, kind) in functions {
            if let Some(signature) = unit.debug_info().and_then(|d| d.functions.get(&hash)) {
                println!("{} = {}", hash, signature);
            } else {
                println!("{} = {}", hash, kind);
            }
        }
    }

    if (args.dump_types || args.dump) && types.peek().is_some() {
        println!("# dynamic types");

        for (hash, ty) in types {
            println!("{} = {}", hash, ty.value_type);
        }
    }

    if strings.peek().is_some() {
        println!("# strings");

        for string in strings {
            println!("{} = {:?}", string.hash(), string);
        }
    }

    if keys.peek().is_some() {
        println!("# object keys");

        for (hash, keys) in keys {
            println!("{} = {:?}", hash, keys);
        }
    }
    Ok(())
}

enum TraceError {
    Io(std::io::Error),
    VmError(runestick::VmError),
}

impl From<std::io::Error> for TraceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Perform a detailed trace of the program.
async fn do_trace(
    execution: &mut VmExecution,
    sources: &rune::Sources,
    dump_stack: bool,
    with_source: bool,
) -> Result<Value, TraceError> {
    use std::io::Write as _;
    let out = std::io::stdout();

    let mut current_frame_len = execution
        .vm()
        .map_err(TraceError::VmError)?
        .call_frames()
        .len();

    loop {
        {
            let vm = execution.vm().map_err(TraceError::VmError)?;
            let mut out = out.lock();

            if let Some((hash, signature)) =
                vm.unit().debug_info().and_then(|d| d.function_at(vm.ip()))
            {
                writeln!(out, "fn {} ({}):", signature, hash)?;
            }

            let debug = vm
                .unit()
                .debug_info()
                .and_then(|d| d.instruction_at(vm.ip()));

            if with_source {
                if let Some((source, span)) =
                    debug.and_then(|d| sources.get(d.source_id).map(|s| (s, d.span)))
                {
                    if let Some((count, line)) = rune::diagnostics::line_for(source.as_str(), span)
                    {
                        writeln!(
                            out,
                            "  {}:{: <3} - {}",
                            source.name(),
                            count + 1,
                            line.trim_end()
                        )?;
                    }
                }
            }

            if let Some(inst) = debug {
                if let Some(label) = &inst.label {
                    writeln!(out, "{}:", label)?;
                }
            }

            if let Some(inst) = vm.unit().instruction_at(vm.ip()) {
                write!(out, "  {:04} = {}", vm.ip(), inst)?;
            } else {
                write!(out, "  {:04} = *out of bounds*", vm.ip())?;
            }

            if let Some(inst) = debug {
                if let Some(comment) = &inst.comment {
                    write!(out, " // {}", comment)?;
                }
            }

            writeln!(out,)?;
        }

        let result = match execution.async_step().await {
            Ok(result) => result,
            Err(e) => return Err(TraceError::VmError(e)),
        };

        let mut out = out.lock();

        if dump_stack {
            let vm = execution.vm().map_err(TraceError::VmError)?;
            let frames = vm.call_frames();

            let stack = vm.stack();

            if current_frame_len != frames.len() {
                if current_frame_len < frames.len() {
                    println!("=> frame {} ({}):", frames.len(), stack.stack_bottom());
                } else {
                    println!("<= frame {} ({}):", frames.len(), stack.stack_bottom());
                }

                current_frame_len = frames.len();
            }

            let values = stack.get(stack.stack_bottom()..).expect("bad stack slice");

            if values.is_empty() {
                println!("    *empty*");
            }

            for (n, value) in values.iter().enumerate() {
                writeln!(out, "    {}+{} = {:?}", stack.stack_bottom(), n, value)?;
            }
        }

        if let Some(result) = result {
            break Ok(result);
        }
    }
}

/// Test if path `a` is newer than path `b`.
fn should_cache_be_used(source: &Path, cached: &Path) -> io::Result<bool> {
    let source = fs::metadata(source)?;

    let cached = match fs::metadata(cached) {
        Ok(cached) => cached,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };

    Ok(source.modified()? < cached.modified()?)
}
