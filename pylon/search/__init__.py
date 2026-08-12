#
# This source file is part of the Pylon open source project.
#
# Copyright (c) 2026 Jaldis B.V.
#
# Licensed under the MIT OR Apache-2.0 license (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://opensource.org/licenses/MIT
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

"""HTTP clients for the external search backends, used on the **query** path.

There is a second, surface-identical pair of these in Rust
(`crates/pylon-workers/src/search_clients.rs`). That duplication is real and
worth understanding before changing either:

- The Rust pair serves the **indexing** path. `SearchIndexWorker` drains
  `_pylon."IndexOutbox"` entirely inside Rust, so reaching back into Python
  to push a document would mean crossing the pyo3 boundary once per document.
- This pair serves the **query** path. `fts::search` compiles to a deferred
  lookup that `pylon.client` resolves by calling the backend directly, and
  `pylon.client` is Python.

The Rust clients already implement the full surface, `search()` included, so
this pair is not covering a capability gap — it is covering a *binding* gap:
nothing exposes `search()` through pyo3 yet. Adding that binding (and routing
`pylon.client`'s deferred-search resolution through it) is what would let
these two modules be deleted, leaving one implementation. Until then, a
change to how documents are indexed or queried has to be made in both.
"""

from .meilisearch import MeilisearchClient
from .opensearch import OpenSearchClient

__all__ = ['MeilisearchClient', 'OpenSearchClient']
