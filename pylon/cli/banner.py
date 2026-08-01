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

import click

_LOGO_COLOR = "\x1b[38;2;204;68;204m"  # #CC44CC
_INFO_COLOR = "\x1b[38;2;136;120;168m"  # #8878A8
_BOLD_RED = "\x1b[1;31m"
_RESET = "\x1b[0m"

_LOGO_LINES = [
    "  ██████╗  ██╗   ██╗██╗      ██████╗ ███╗  ██╗",
    "  ██╔══██╗ ╚██╗ ██╔╝██║     ██╔═══██╗████╗ ██║",
    "  ██████╔╝  ╚████╔╝ ██║     ██║   ██║██╔██╗██║",
    " ██╔═══╝    ╚██╔╝  ██║     ██║   ██║██║╚████║",
    " ██║         ██║   ███████╗╚██████╔╝██║ ╚███║",
    " ╚═╝         ╚═╝   ╚══════╝ ╚═════╝ ╚═╝  ╚══╝",
]


def print_banner(*, info_line: str | None = None) -> None:
    """Print the Pylon ASCII logo followed by an optional info line.

    Args:
        info_line: Text rendered in info colour below the version line.
                   Omit for the standard REPL banner.
    """
    try:
        from importlib.metadata import version as _version

        v = _version("pylon")
    except Exception:
        v = "(development)"

    for line in _LOGO_LINES:
        click.echo(f"{_LOGO_COLOR}{line}{_RESET}")
    click.echo()
    click.echo(f"{_INFO_COLOR}Pylon {v}{_RESET}")

    if info_line is not None:
        click.echo(f"{_INFO_COLOR}{info_line}{_RESET}")

    click.echo()
