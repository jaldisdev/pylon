//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();

    // Parse semver: "MAJOR.MINOR.PATCH[-PRERELEASE]"
    let (version_core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version.as_str(), None),
    };

    let mut nums = version_core.splitn(3, '.');
    let major: u64 = nums.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u64 = nums.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    let (stage, stage_no) = match pre {
        Some(p) if p.starts_with("alpha") => {
            let n: u64 = p.trim_start_matches("alpha.").parse().unwrap_or(0);
            ("alpha", n)
        }
        Some(p) if p.starts_with("beta") => {
            let n: u64 = p.trim_start_matches("beta.").parse().unwrap_or(0);
            ("beta", n)
        }
        Some(p) if p.starts_with("rc") => {
            let n: u64 = p.trim_start_matches("rc.").parse().unwrap_or(0);
            ("rc", n)
        }
        Some(_) => ("dev", 0),
        None if major == 0 => ("dev", 0),
        None => ("final", 0),
    };

    println!(
        "cargo:rustc-env=PYLON_VERSION_ROW=ROW({}::int8, {}::int8, '{}'::text, {}::int8, ARRAY[]::text[])",
        major, minor, stage, stage_no
    );
    println!("cargo:rerun-if-changed=Cargo.toml");
}
