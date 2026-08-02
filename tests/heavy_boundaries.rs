use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use minicore_runtime::wire::SessionId;
use minicore_runtime::wire::heavy_test_support::{
    ConversationScanBoundaryError, ConversationScanBoundarySummary, ConversationScanFaultCounters,
    scan_conversation_file,
};
use serde::Deserialize;
use serde_json::Value;

const HEADER: &[u8] =
    include_bytes!("../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl");
const MAX_CONVERSATION_STRING_BYTES: usize = 524_288;
const ROOT_ENTRY_COUNTER: u128 = 1;
const RECOVERY_ROOT_ENTRY_COUNTER: u128 = 2;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct BoundaryRecipes {
    cases: Vec<BoundaryCase>,
}

#[derive(Deserialize)]
struct BoundaryCase {
    name: String,
    scope: String,
    #[serde(rename = "targetBytesExcludingLineEnding")]
    target_content_bytes: Option<usize>,
    #[serde(rename = "targetBytes")]
    target_file_bytes: Option<u64>,
    #[serde(rename = "targetCount")]
    target_count: Option<u64>,
    #[serde(default)]
    generator: BoundaryGenerator,
    expected: Value,
}

#[derive(Default, Deserialize)]
struct BoundaryGenerator {
    kind: String,
    #[serde(rename = "uniqueEntryIds")]
    unique_entry_ids: Option<bool>,
}

struct TempConversationFile {
    path: PathBuf,
    file: Option<File>,
}

impl TempConversationFile {
    fn new(label: &str) -> Self {
        for _ in 0..128 {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-heavy-{label}-{}-{sequence}.jsonl",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Self {
                        path,
                        file: Some(file),
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => panic!("could not create generated heavy boundary file"),
            }
        }
        panic!("could not allocate a unique generated heavy boundary file");
    }

    fn writer(&mut self) -> BufWriter<&mut File> {
        BufWriter::new(
            self.file
                .as_mut()
                .expect("generated heavy boundary file must remain open while writing"),
        )
    }

    fn scan(&mut self) -> Result<ConversationScanBoundarySummary, ConversationScanBoundaryError> {
        self.file
            .as_mut()
            .expect("generated heavy boundary file must remain open while scanning")
            .flush()
            .expect("generated heavy boundary file must flush before scan");
        let file = File::open(&self.path).expect("generated heavy boundary file must reopen");
        scan_conversation_file(file, session_id())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempConversationFile {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn recipes() -> BoundaryRecipes {
    serde_json::from_str(include_str!(
        "../docs/fixtures/wire-v1/recipes/boundary-cases.json"
    ))
    .expect("boundary recipe document must be valid JSON")
}

fn case<'a>(recipes: &'a BoundaryRecipes, name: &str) -> &'a BoundaryCase {
    recipes
        .cases
        .iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("missing authoritative boundary recipe {name}"))
}

fn session_id() -> SessionId {
    "ses_11111111111111111111111111111111"
        .parse()
        .expect("fixture session ID is valid")
}

fn runtime_id(prefix: &str, counter: u128) -> String {
    assert_ne!(
        counter, 0,
        "generated runtime IDs must not use all-zero payloads"
    );
    format!("{prefix}_{counter:032x}")
}

fn canonical_input_user_entry_line(counter: u128, parent: Option<u128>) -> Vec<u8> {
    let entry_id = runtime_id("ent", counter);
    let parent_id = parent.map_or_else(
        || "null".to_owned(),
        |parent| format!("\"{}\"", runtime_id("ent", parent)),
    );
    let turn_id = runtime_id("trn", counter);
    let item_id = runtime_id("itm", counter);
    format!(
        "{{\"type\":\"entry\",\"data\":{{\"entryId\":\"{entry_id}\",\"parentId\":{parent_id},\"sessionId\":\"ses_11111111111111111111111111111111\",\"turnId\":\"{turn_id}\",\"timestamp\":\"2026-07-31T12:00:01.000Z\",\"body\":{{\"type\":\"user_message\",\"data\":{{\"itemId\":\"{item_id}\",\"source\":\"input\",\"content\":{{\"parts\":[{{\"type\":\"text\",\"data\":{{\"text\":\"x\"}}}}],\"contributionStamps\":[]}}}}}}}}}}"
    )
    .into_bytes()
}

fn canonical_root_input_user_entry_line(counter: u128) -> Vec<u8> {
    canonical_input_user_entry_line(counter, None)
}

fn write_repeated_byte(writer: &mut impl Write, byte: u8, mut count: u64) {
    let bytes = [byte; 65_536];
    while count != 0 {
        let chunk = usize::try_from(count.min(bytes.len() as u64))
            .expect("generated chunk length fits usize");
        writer
            .write_all(&bytes[..chunk])
            .expect("generated boundary bytes must write");
        count -= u64::try_from(chunk).expect("generated chunk length fits u64");
    }
}

fn write_header(writer: &mut impl Write) {
    writer
        .write_all(HEADER)
        .expect("generated Header must write");
}

fn write_canonical_entry(writer: &mut impl Write, counter: u128, parent: Option<u128>) {
    let entry = canonical_input_user_entry_line(counter, parent);
    writer
        .write_all(&entry)
        .expect("generated entry record must write");
    writer
        .write_all(b"\n")
        .expect("generated entry newline must write");
}

fn write_canonical_root_input_user_entry(writer: &mut impl Write, counter: u128) {
    let entry = canonical_root_input_user_entry_line(counter);
    writer
        .write_all(&entry)
        .expect("generated entry record must write");
    writer
        .write_all(b"\n")
        .expect("generated entry newline must write");
}

fn write_bounded_unknown_padding_entry(writer: &mut impl Write, content_bytes: usize) {
    let entry = canonical_root_input_user_entry_line(ROOT_ENTRY_COUNTER);
    let prefix = &entry[..entry.len() - 2];
    let suffix = b"]}}";
    let field_prefix = b",\"futurePadding\":[";
    let fixed_bytes = prefix
        .len()
        .checked_add(field_prefix.len())
        .and_then(|value| value.checked_add(suffix.len()))
        .and_then(|value| value.checked_add(5))
        .expect("generated entry byte accounting must not overflow");
    let string_bytes = content_bytes
        .checked_sub(fixed_bytes)
        .expect("authoritative entry target must fit the canonical prefix");
    let first_string_bytes = string_bytes.min(MAX_CONVERSATION_STRING_BYTES);
    let second_string_bytes = string_bytes - first_string_bytes;
    assert!(
        second_string_bytes <= MAX_CONVERSATION_STRING_BYTES,
        "authoritative entry target must use only bounded additive strings"
    );

    writer
        .write_all(prefix)
        .and_then(|_| writer.write_all(field_prefix))
        .and_then(|_| writer.write_all(b"\""))
        .expect("generated padded entry prefix must write");
    write_repeated_byte(
        writer,
        b'x',
        u64::try_from(first_string_bytes).expect("padding fits u64"),
    );
    writer
        .write_all(b"\",\"")
        .expect("generated padded entry separator must write");
    write_repeated_byte(
        writer,
        b'x',
        u64::try_from(second_string_bytes).expect("padding fits u64"),
    );
    writer
        .write_all(b"\"")
        .and_then(|_| writer.write_all(suffix))
        .and_then(|_| writer.write_all(b"\n"))
        .expect("generated padded entry suffix must write");
}

fn write_entry_chain(writer: &mut impl Write, count: u64) {
    let mut previous = None;
    for counter in 1..=u128::from(count) {
        write_canonical_entry(writer, counter, previous);
        previous = Some(counter);
    }
}

fn no_faults() -> ConversationScanFaultCounters {
    ConversationScanFaultCounters::default()
}

fn only_oversized_line() -> ConversationScanFaultCounters {
    ConversationScanFaultCounters {
        oversized_line: 1,
        ..no_faults()
    }
}

#[test]
fn generated_real_one_mebibyte_entry_boundaries_stream_authoritative_unknown_padding() {
    let recipes = recipes();
    let exact_recipe = case(&recipes, "entry_line_boundary");
    let plus_one_recipe = case(&recipes, "entry_line_oversized");
    assert_eq!(exact_recipe.scope, "conversation_entry_line");
    assert_eq!(
        exact_recipe.generator.kind,
        "entryWithBoundedUnknownPadding"
    );
    assert_eq!(exact_recipe.expected["lineAccepted"], true);
    assert_eq!(plus_one_recipe.scope, "conversation_entry_line");
    assert_eq!(
        plus_one_recipe.generator.kind,
        "entryWithBoundedUnknownPaddingThenCanonicalEntry"
    );
    assert_eq!(plus_one_recipe.expected["diagnostic"], "oversized_line");
    assert_eq!(
        plus_one_recipe.expected["followingCanonicalEntryAccepted"],
        true
    );
    let exact_bytes = exact_recipe
        .target_content_bytes
        .expect("entry boundary recipe has an exact content target");
    let plus_one_bytes = plus_one_recipe
        .target_content_bytes
        .expect("entry oversize recipe has an exact content target");
    assert_eq!(plus_one_bytes, exact_bytes + 1);

    let mut exact = TempConversationFile::new("entry-exact");
    {
        let mut writer = exact.writer();
        write_header(&mut writer);
        write_bounded_unknown_padding_entry(&mut writer, exact_bytes);
        writer.flush().expect("exact entry file must flush");
    }
    assert_eq!(
        exact.scan(),
        Ok(ConversationScanBoundarySummary {
            complete_entries: 1,
            faults: no_faults(),
            saw_partial_tail: false,
        })
    );
    drop(exact);

    let mut plus_one = TempConversationFile::new("entry-plus-one");
    {
        let mut writer = plus_one.writer();
        write_header(&mut writer);
        write_bounded_unknown_padding_entry(&mut writer, plus_one_bytes);
        // The padded fixture is the counter-1 entry in this recipe. Recovery must append a
        // distinct root with fresh Entry, Turn, and Item IDs rather than repeat that identity.
        write_canonical_root_input_user_entry(&mut writer, RECOVERY_ROOT_ENTRY_COUNTER);
        writer.flush().expect("oversized entry file must flush");
    }
    assert_eq!(
        plus_one.scan(),
        Ok(ConversationScanBoundarySummary {
            complete_entries: 2,
            faults: only_oversized_line(),
            saw_partial_tail: false,
        })
    );
}

#[test]
fn generated_real_one_million_complete_entry_chain_boundaries_stream() {
    let recipes = recipes();
    let exact_recipe = case(&recipes, "complete_entry_count_boundary");
    let plus_one_recipe = case(&recipes, "complete_entry_count_oversized");
    assert_eq!(exact_recipe.scope, "conversation_complete_entries");
    assert_eq!(exact_recipe.generator.kind, "streamCanonicalEntryChain");
    assert_eq!(exact_recipe.generator.unique_entry_ids, Some(true));
    assert_eq!(exact_recipe.expected["countAccepted"], true);
    assert_eq!(plus_one_recipe.scope, "conversation_complete_entries");
    assert_eq!(plus_one_recipe.generator.kind, "streamCanonicalEntryChain");
    assert_eq!(plus_one_recipe.generator.unique_entry_ids, Some(true));
    assert_eq!(plus_one_recipe.expected["error"], "HistoryTooLarge");
    let exact_count = exact_recipe
        .target_count
        .expect("entry-count boundary recipe has a target count");
    let plus_one_count = plus_one_recipe
        .target_count
        .expect("entry-count oversize recipe has a target count");
    assert_eq!(plus_one_count, exact_count + 1);
    assert_eq!(plus_one_recipe.expected["failureAtEntry"], plus_one_count);

    let mut exact = TempConversationFile::new("entry-count-exact");
    {
        let mut writer = exact.writer();
        write_header(&mut writer);
        write_entry_chain(&mut writer, exact_count);
        writer.flush().expect("exact entry-count file must flush");
    }
    assert_eq!(
        exact.scan(),
        Ok(ConversationScanBoundarySummary {
            complete_entries: exact_count,
            faults: no_faults(),
            saw_partial_tail: false,
        })
    );
    drop(exact);

    let mut plus_one = TempConversationFile::new("entry-count-plus-one");
    {
        let mut writer = plus_one.writer();
        write_header(&mut writer);
        write_entry_chain(&mut writer, plus_one_count);
        writer
            .flush()
            .expect("plus-one entry-count file must flush");
    }
    assert_eq!(
        plus_one.scan(),
        Err(ConversationScanBoundaryError::HistoryTooLarge)
    );
}

#[test]
fn generated_real_one_gibibyte_file_boundaries_keep_the_canonical_prefix_before_ascii_tail() {
    let recipes = recipes();
    let exact_recipe = case(&recipes, "file_boundary");
    let plus_one_recipe = case(&recipes, "file_oversized");
    assert_eq!(exact_recipe.scope, "conversation_file");
    assert_eq!(
        exact_recipe.generator.kind,
        "canonicalHeaderAndOneEntryThenAsciiPartialTail"
    );
    assert_eq!(
        exact_recipe.expected["tailTruncationEvaluatedAfterFileCap"],
        true
    );
    assert_eq!(plus_one_recipe.scope, "conversation_file");
    assert_eq!(
        plus_one_recipe.generator.kind,
        "canonicalHeaderAndOneEntryThenAsciiPartialTail"
    );
    assert_eq!(plus_one_recipe.expected["error"], "HistoryTooLarge");
    let exact_bytes = exact_recipe
        .target_file_bytes
        .expect("file boundary recipe has an exact byte target");
    let plus_one_bytes = plus_one_recipe
        .target_file_bytes
        .expect("file oversize recipe has an exact byte target");
    assert_eq!(plus_one_bytes, exact_bytes + 1);

    let canonical_entry = canonical_root_input_user_entry_line(ROOT_ENTRY_COUNTER);
    let header_bytes = u64::try_from(HEADER.len()).expect("Header length fits u64");
    let entry_bytes = u64::try_from(canonical_entry.len()).expect("entry length fits u64");
    let prefix_bytes = header_bytes
        .checked_add(entry_bytes)
        .and_then(|bytes| bytes.checked_add(1))
        .expect("canonical file prefix fits u64");

    let mut exact = TempConversationFile::new("file-exact");
    {
        let mut writer = exact.writer();
        write_header(&mut writer);
        write_canonical_root_input_user_entry(&mut writer, ROOT_ENTRY_COUNTER);
        write_repeated_byte(&mut writer, b'x', exact_bytes - prefix_bytes);
        writer.flush().expect("exact file cap fixture must flush");
    }
    assert_eq!(
        fs::metadata(exact.path())
            .expect("exact file cap fixture metadata must be available")
            .len(),
        exact_bytes
    );
    assert_eq!(
        exact.scan(),
        Ok(ConversationScanBoundarySummary {
            complete_entries: 1,
            faults: no_faults(),
            saw_partial_tail: true,
        })
    );
    drop(exact);

    let mut plus_one = TempConversationFile::new("file-plus-one");
    {
        let mut writer = plus_one.writer();
        write_header(&mut writer);
        write_canonical_root_input_user_entry(&mut writer, ROOT_ENTRY_COUNTER);
        write_repeated_byte(&mut writer, b'x', plus_one_bytes - prefix_bytes);
        writer
            .flush()
            .expect("plus-one file cap fixture must flush");
    }
    assert_eq!(
        fs::metadata(plus_one.path())
            .expect("plus-one file cap fixture metadata must be available")
            .len(),
        plus_one_bytes
    );
    assert_eq!(
        plus_one.scan(),
        Err(ConversationScanBoundaryError::HistoryTooLarge)
    );
}
