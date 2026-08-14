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

pub mod analyze;
pub mod cast;
pub mod diff;
pub mod error;
pub mod export;
pub mod introspect;
pub mod ir;
pub mod migrate;
pub mod migration;
pub mod parse;
pub mod query;
pub mod schema;
pub mod shape_id;
pub mod sql;
pub mod stdlib;
pub mod validate;

#[cfg(test)]
mod sync_assertions {
    fn assert_sync<T: Sync + Send>() {}

    /// The types `pylon-py` wants to hold across a `py.detach()` — if any of
    /// these stopped being `Send + Sync`, the GIL-releasing wrappers there
    /// would stop compiling, and this says why.
    #[test]
    fn types_crossed_by_gil_releasing_calls_are_send_and_sync() {
        assert_sync::<crate::schema::SchemaDescriptor>();
        assert_sync::<crate::query::CompiledQuery>();
        assert_sync::<crate::ir::SessionConfig>();
    }
}
