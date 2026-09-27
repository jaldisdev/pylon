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

from __future__ import annotations

from importlib.metadata import version as _dist_version

import pylon._core
from click.testing import CliRunner

from pylon.cli.commands.version import version as version_cmd


class TestVersionReporting:
    # The distribution is published as `pylon-db`, and the name every reporting
    # path looks up has to be that one. Asking for `pylon` raises
    # PackageNotFoundError, which these paths catch and turn into
    # "(development)" — so getting the name wrong is invisible until someone
    # runs an installed Pylon and is told it is a development checkout.
    def test_cli_reports_the_installed_version(self):
        result = CliRunner().invoke(version_cmd, [])
        assert result.exit_code == 0
        assert result.output.strip() == f'Pylon {_dist_version("pylon-db")}'

    def test_compiled_core_carries_its_version(self):
        assert pylon._core.__version__ == _dist_version('pylon-db')
