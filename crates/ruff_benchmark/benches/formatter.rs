use std::path::Path;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use ruff_benchmark::{TestCase, TestCaseSpeed, TestFile, TestFileDownloadError};
use ruff_python_formatter::{format_node, PyFormatOptions};
use ruff_python_index::CommentRangesBuilder;
use ruff_python_parser::lexer::lex;
use ruff_python_parser::{parse_tokens, Mode};

#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(
    not(target_os = "windows"),
    not(target_os = "openbsd"),
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64"
    )
))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn create_test_cases() -> Result<Vec<TestCase>, TestFileDownloadError> {
    Ok(vec![
        TestCase::fast(TestFile::try_download("numpy/globals.py", "https://raw.githubusercontent.com/numpy/numpy/89d64415e349ca75a25250f22b874aa16e5c0973/numpy/_globals.py")?),
        TestCase::normal(TestFile::try_download(
            "pydantic/types.py",
            "https://raw.githubusercontent.com/pydantic/pydantic/83b3c49e99ceb4599d9286a3d793cea44ac36d4b/pydantic/types.py",
        )?),
        TestCase::normal(TestFile::try_download("numpy/ctypeslib.py", "https://raw.githubusercontent.com/numpy/numpy/e42c9503a14d66adfd41356ef5640c6975c45218/numpy/ctypeslib.py")?),
        TestCase::slow(TestFile::try_download(
            "large/dataset.py",
            "https://raw.githubusercontent.com/DHI/mikeio/b7d26418f4db2909b0aa965253dbe83194d7bb5b/tests/test_dataset.py",
        )?),
    ])
}

fn benchmark_formatter(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("formatter");
    let test_cases = create_test_cases().unwrap();

    for case in test_cases {
        group.throughput(Throughput::Bytes(case.code().len() as u64));

        group.measurement_time(match case.speed() {
            TestCaseSpeed::Fast => Duration::from_secs(5),
            TestCaseSpeed::Normal => Duration::from_secs(10),
            TestCaseSpeed::Slow => Duration::from_secs(20),
        });

        group.bench_with_input(
            BenchmarkId::from_parameter(case.name()),
            &case,
            |b, case| {
                let mut tokens = Vec::new();
                let mut comment_ranges = CommentRangesBuilder::default();

                for result in lex(case.code(), Mode::Module) {
                    let (token, range) = result.expect("Input to be a valid python program.");

                    comment_ranges.visit_token(&token, range);
                    tokens.push(Ok((token, range)));
                }

                let comment_ranges = comment_ranges.finish();

                // Parse the AST.
                let python_ast = parse_tokens(tokens, Mode::Module, "<filename>")
                    .expect("Input to be a valid python program");

                b.iter(|| {
                    let options = PyFormatOptions::from_extension(Path::new(case.name()));
                    let formatted = format_node(&python_ast, &comment_ranges, case.code(), options)
                        .expect("Formatting to succeed");

                    formatted.print().expect("Printing to succeed")
                });
            },
        );
    }

    group.finish();
}

criterion_group!(formatter, benchmark_formatter);
criterion_main!(formatter);
