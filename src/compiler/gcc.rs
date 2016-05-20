// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use ::compiler::{
    Compiler,
    CompilerArguments,
    ParsedArguments,
    ProcessOutput,
    run_compiler,
};
use log::LogLevel::Trace;
use mock_command::{
    CommandCreator,
    CommandCreatorSync,
};
use std::collections::HashMap;
use std::io::{
    self,
    Error,
    ErrorKind,
};
use std::path::Path;
use std::process;

pub fn parse_arguments(arguments : &[String]) -> CompilerArguments {
    trace!("gcc::parse_arguments");
    let mut output_arg = None;
    let mut input_arg = None;
    let mut dep_target = None;
    let mut common_args = vec!();
    let mut preprocessor_args = vec!();
    let mut compilation = false;
    let mut split_dwarf = false;
    let mut need_explicit_dep_target = false;

    let mut it = arguments.iter();
    loop {
        match it.next() {
            Some(arg) => {
                match arg.as_ref() {
                    "-c" => compilation = true,
                    "-o" => output_arg = it.next(),
                    "-gsplit-dwarf" => {
                        split_dwarf = true;
                        common_args.push(arg.clone());
                    }
                    // Arguments that take a value.
                    // -MF and -MQ are in this set but are handled separately
                    // because they are also preprocessor options.
                    "--param" | "-A" | "-D" | "-F" | "-G" | "-I" | "-L" |
                    "-U" | "-V" | "-Xassembler" | "-Xlinker" |
                    "-Xpreprocessor" | "-aux-info" | "-b" | "-idirafter" |
                    "-iframework" | "-imacros" | "-imultilib" | "-include" |
                    "-install_name" | "-iprefix" | "-iquote" | "-isysroot" |
                    "-isystem" | "-iwithprefix" | "-iwithprefixbefore" |
                    "-u" => {
                        common_args.push(arg.clone());
                        if let Some(arg_val) = it.next() {
                            common_args.push(arg_val.clone());
                        }
                    },
                    "-MF" | "-MQ" => {
                        preprocessor_args.push(arg.clone());
                        if let Some(arg_val) = it.next() {
                            preprocessor_args.push(arg_val.clone());
                        }
                    }
                    "-MT" => dep_target = it.next(),
                    // Can't cache PGO profiled output.
                    "-fprofile-use" => return CompilerArguments::CannotCache,
                    // Can't cache commandlines using a response file.
                    v @ _ if v.starts_with('@') => return CompilerArguments::CannotCache,
                    "-M" | "-MM" | "-MD" | "-MMD" => {
                        // If one of the above options is on the command line, we'll
                        // need -MT on the preprocessor command line, whether it's
                        // been passed already or not
                        need_explicit_dep_target = true;
                        preprocessor_args.push(arg.clone());
                    }
                    // Other options.
                    v @ _ if v.starts_with('-') && v.len() > 1 => {
                        common_args.push(arg.clone());
                    }
                    // Anything else is an input file.
                    v @ _ => {
                        if input_arg.is_some() || v == "-" {
                            // Can't cache compilations with multiple inputs
                            // or compilation from stdin.
                            return CompilerArguments::CannotCache;
                        }
                        input_arg = Some(v);
                    }
                };
            },
            None => break,
        }
    }
    // We only support compilation.
    if !compilation {
        return CompilerArguments::NotCompilation;
    }
    let (input, extension) = match input_arg {
        Some(i) => {
            // When compiling from the preprocessed output given as stdin, we need
            // to explicitly pass its file type.
            match Path::new(i).extension().and_then(|e| e.to_str()) {
                Some(e @ "c") | Some(e @ "cc") | Some(e @ "cpp") | Some(e @ "cxx") => (i.to_owned(), e.to_owned()),
                e @ _ => {
                    trace!("Unknown source extension: {}", e.unwrap_or("(None)"));
                    return CompilerArguments::CannotCache;
                }
            }
        }
        // We can't cache compilation without an input.
        None => return CompilerArguments::CannotCache,
    };
    let mut outputs = HashMap::new();
    match output_arg {
        // We can't cache compilation that doesn't go to a file
        None => return CompilerArguments::CannotCache,
        Some(o) => {
            outputs.insert("obj", o.to_owned());
            if split_dwarf {
                Path::new(o)
                    .with_extension("dwo")
                    //TODO: should really be dealing with OsStr everywhere.
                    .to_str()
                    .and_then(|dwo| outputs.insert("dwo", dwo.to_owned()));
            }
            if need_explicit_dep_target {
                preprocessor_args.extend_from_slice(&["-MT".to_owned(), dep_target.unwrap_or(o).to_owned()]);
            }
        }
    }

    CompilerArguments::Ok(ParsedArguments {
        input: input,
        extension: extension,
        outputs: outputs,
        preprocessor_args: preprocessor_args,
        common_args: common_args,
    })
}

pub fn preprocess<T : CommandCreatorSync>(creator: T, compiler: &Compiler, parsed_args: &ParsedArguments, cwd: &str) -> io::Result<process::Output> {
    trace!("preprocess");
    let mut cmdline = vec!["-E".to_string(), parsed_args.input.clone()];
    cmdline.extend(parsed_args.preprocessor_args.clone());
    cmdline.extend(parsed_args.common_args.clone());

    if log_enabled!(Trace) {
        let cmd_str = cmdline.join(" ");
        trace!("preprocess: `{} {}` in '{}'", compiler.executable, cmd_str, cwd);
    }
    run_compiler(creator.clone(), &compiler.executable, &cmdline, &cwd, None, ProcessOutput::Capture)
}

pub fn compile<T : CommandCreatorSync>(creator: T, compiler: &Compiler, preprocessor_output: Vec<u8>, parsed_args: &ParsedArguments, cwd: &str) -> io::Result<process::Output> {
    trace!("compile");
    let output = try!(parsed_args.outputs.get("obj").ok_or(Error::new(ErrorKind::Other, "Missing object file output")));
    //FIXME: sort out the ownership nonsense here
    let mut cmdline = vec![
        "-c".to_string(),
        "-x".to_string(),
        match parsed_args.extension.as_ref() {
            "c" => "cpp-output".to_string(),
            "cc" | "cpp" | "cxx" => "c++-cpp-output".to_string(),
            e @ _ => {
                error!("gcc::compile: Got an unexpected file extension {}", e);
                return Err(Error::new(ErrorKind::Other, "Unexpected file extension"));
            }
        },
        "-".to_string(),
        "-o".to_string(), output.clone(),
        ];
    cmdline.extend(parsed_args.common_args.clone());
    if log_enabled!(Trace) {
        let cmd_str = cmdline.join(" ");
        trace!("compile: `{} {}` in '{}'", compiler.executable, cmd_str, cwd);
    }
    run_compiler(creator.clone(), &compiler.executable, &cmdline, &cwd, Some(preprocessor_output), ProcessOutput::Capture)
}

#[cfg(test)]
mod test {
    use super::*;
    use ::compiler::*;

    #[test]
    fn test_parse_arguments_simple() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-o", "foo.o"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert!(common_args.is_empty());
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_split_dwarf() {
        match parse_arguments(&stringvec!["-gsplit-dwarf", "-c", "foo.cpp", "-o", "foo.o"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.cpp", input);
                assert_eq!("cpp", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"), ("dwo", "foo.dwo"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(2, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert_eq!(stringvec!["-gsplit-dwarf"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_extra() {
        match parse_arguments(&stringvec!["-c", "foo.cc", "-fabc", "-o", "foo.o", "-mxyz"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.cc", input);
                assert_eq!("cc", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert_eq!(stringvec!["-fabc", "-mxyz"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_values() {
        match parse_arguments(&stringvec!["-c", "foo.cxx", "-fabc", "-I", "include", "-o", "foo.o", "-include", "file"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.cxx", input);
                assert_eq!("cxx", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert_eq!(stringvec!["-fabc", "-I", "include", "-include", "file"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_preprocessor_args() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-fabc", "-MF", "file", "-o", "foo.o", "-MQ", "abc"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert_eq!(stringvec!["-MF", "file", "-MQ", "abc"], preprocessor_args);
                assert_eq!(stringvec!["-fabc"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_explicit_dep_target() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-MT", "depfile", "-fabc", "-MF", "file", "-o", "foo.o"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert_eq!(stringvec!["-MF", "file"], preprocessor_args);
                assert_eq!(stringvec!["-fabc"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_explicit_dep_target_needed() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-MT", "depfile", "-fabc", "-MF", "file", "-o", "foo.o", "-MD"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert_eq!(stringvec!["-MF", "file", "-MD", "-MT", "depfile"], preprocessor_args);
                assert_eq!(stringvec!["-fabc"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_dep_target_needed() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-fabc", "-MF", "file", "-o", "foo.o", "-MD"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.o"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert_eq!(stringvec!["-MF", "file", "-MD", "-MT", "foo.o"], preprocessor_args);
                assert_eq!(stringvec!["-fabc"], common_args);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_empty_args() {
        assert_eq!(CompilerArguments::NotCompilation,
                   parse_arguments(&vec!()));
    }

    #[test]
    fn test_parse_arguments_not_compile() {
        assert_eq!(CompilerArguments::NotCompilation,
                   parse_arguments(&stringvec!["-o", "foo"]));
    }

    #[test]
    fn test_parse_arguments_too_many_inputs() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "-o", "foo.o", "bar.c"]));
    }

    #[test]
    fn test_parse_arguments_pgo() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "-fprofile-use", "-o", "foo.o"]));
    }


    #[test]
    fn test_parse_arguments_response_file() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "@foo", "-o", "foo.o"]));
    }
}
