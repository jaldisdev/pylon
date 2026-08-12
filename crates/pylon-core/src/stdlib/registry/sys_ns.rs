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

use super::{Array, FnDescriptor, Int64, Str, Tuple};
use super::{B, E, f};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f("sys", "get_current_database", vec![], Str, B("current_database")),
        f(
            "sys",
            "get_version_as_str",
            vec![],
            Str,
            E(concat!("'", env!("CARGO_PKG_VERSION"), "'")),
        ),
        f(
            "sys",
            "get_version",
            vec![],
            Tuple(vec![Int64, Int64, Str, Int64, Array(Box::new(Str))]),
            E(env!("PYLON_VERSION_ROW")),
        ),
    ]
}
