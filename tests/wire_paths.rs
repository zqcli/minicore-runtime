use std::str::FromStr;

use minicore_runtime::wire::{
    CanonicalFileUri, FileUriFamily, ProtocolLimits, WorkspaceRelativePath,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileUriVectors {
    version: u32,
    target: String,
    valid: Vec<ValidFileUri>,
    invalid: Vec<InvalidFileUri>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValidFileUri {
    wire: String,
    family: String,
    authority: Option<String>,
    decoded_path: String,
}

#[derive(Deserialize)]
struct InvalidFileUri {
    wire: String,
    reason: String,
}

#[test]
fn canonical_file_uri_matches_authoritative_vectors() {
    let vectors: FileUriVectors = serde_json::from_str(include_str!(
        "../docs/fixtures/wire-v1/public/carriers/file-uri.json"
    ))
    .unwrap();

    assert_eq!(vectors.version, 1);
    assert_eq!(vectors.target, "CanonicalFileUri");

    for vector in vectors.valid {
        let uri = CanonicalFileUri::from_str(&vector.wire)
            .unwrap_or_else(|error| panic!("rejected valid {}: {error}", vector.wire));
        let expected_family = match vector.family.as_str() {
            "posix" => FileUriFamily::Posix,
            "drive" => FileUriFamily::Drive,
            "unc" => FileUriFamily::Unc,
            other => panic!("unknown fixture family {other}"),
        };
        assert_eq!(uri.family(), expected_family, "{}", vector.wire);
        assert_eq!(
            uri.authority(),
            vector.authority.as_deref(),
            "{}",
            vector.wire
        );
        assert_eq!(uri.decoded_path(), vector.decoded_path, "{}", vector.wire);
        assert_eq!(uri.as_str(), vector.wire);
        assert_eq!(
            serde_json::to_string(&uri).unwrap(),
            format!("\"{}\"", vector.wire)
        );
    }

    for vector in vectors.invalid {
        assert!(
            CanonicalFileUri::from_str(&vector.wire).is_err(),
            "accepted {} ({})",
            vector.wire,
            vector.reason
        );
    }
    let max_uri_bytes = ProtocolLimits::v1_0().workspace.max_absolute_path_uri_bytes as usize;
    let max_uri = format!("file:///{}", "x".repeat(max_uri_bytes - "file:///".len()));
    assert_eq!(max_uri.len(), max_uri_bytes);
    assert!(CanonicalFileUri::from_str(&max_uri).is_ok());
    assert!(CanonicalFileUri::from_str(&format!("{max_uri}x")).is_err());
    assert_eq!(
        serde_json::from_str::<CanonicalFileUri>(&serde_json::to_string(&max_uri).unwrap())
            .unwrap()
            .as_str(),
        max_uri
    );
    assert!(serde_json::from_str::<CanonicalFileUri>("1").is_err());

    let label_63 = "a".repeat(63);
    assert!(CanonicalFileUri::from_str(&format!("file://{label_63}/share")).is_ok());
    assert!(CanonicalFileUri::from_str(&format!("file://{}a/share", label_63)).is_err());
    let authority_253 = format!(
        "{}.{}.{}.{}",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61)
    );
    assert_eq!(authority_253.len(), 253);
    assert!(CanonicalFileUri::from_str(&format!("file://{authority_253}/share")).is_ok());
    assert!(CanonicalFileUri::from_str(&format!("file://{authority_253}e/share")).is_err());
}

#[test]
fn relative_workspace_path_is_forward_slash_utf8_without_traversal() {
    for valid in ["", "src", "src/lib.rs", "项目/说明.md"] {
        let path = WorkspaceRelativePath::from_str(valid).unwrap();
        assert_eq!(path.as_str(), valid);
        assert_eq!(
            serde_json::to_string(&path).unwrap(),
            format!("\"{valid}\"")
        );
    }

    for invalid in [
        "/src",
        "src/",
        "src//lib.rs",
        "src\\lib.rs",
        ".",
        "..",
        "src/../secret",
        "C:/work",
        "//server/share",
        "nul\0byte",
        "line\nbreak",
        "tab\tpath",
        "delete\u{007f}path",
        "c1\u{0085}path",
    ] {
        assert!(
            WorkspaceRelativePath::from_str(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }

    let redacted = WorkspaceRelativePath::from_str("private/source").unwrap();
    assert!(!format!("{redacted}").contains("private/source"));
    assert!(!format!("{redacted:?}").contains("private/source"));

    let limits = ProtocolLimits::v1_0().workspace;
    let max_bytes = "x".repeat(limits.max_relative_path_bytes as usize);
    assert!(WorkspaceRelativePath::from_str(&max_bytes).is_ok());
    assert!(WorkspaceRelativePath::from_str(&format!("{max_bytes}x")).is_err());

    let max_multibyte_bytes = "é".repeat(limits.max_relative_path_bytes as usize / 2);
    assert_eq!(
        max_multibyte_bytes.len(),
        limits.max_relative_path_bytes as usize
    );
    assert!(WorkspaceRelativePath::from_str(&max_multibyte_bytes).is_ok());
    assert!(WorkspaceRelativePath::from_str(&format!("{max_multibyte_bytes}é")).is_err());

    let max_segments = std::iter::repeat_n("x", limits.max_relative_path_segments as usize)
        .collect::<Vec<_>>()
        .join("/");
    assert!(WorkspaceRelativePath::from_str(&max_segments).is_ok());
    let too_many_segments =
        std::iter::repeat_n("x", limits.max_relative_path_segments as usize + 1)
            .collect::<Vec<_>>()
            .join("/");
    assert!(WorkspaceRelativePath::from_str(&too_many_segments).is_err());

    let json = serde_json::to_string(&max_bytes).unwrap();
    assert_eq!(
        serde_json::from_str::<WorkspaceRelativePath>(&json)
            .unwrap()
            .as_str(),
        max_bytes
    );
    assert!(serde_json::from_str::<WorkspaceRelativePath>("1").is_err());
    assert!(
        serde_json::from_str::<WorkspaceRelativePath>(
            &serde_json::to_string(&"x".repeat(limits.max_relative_path_bytes as usize + 1))
                .unwrap()
        )
        .is_err()
    );
}
