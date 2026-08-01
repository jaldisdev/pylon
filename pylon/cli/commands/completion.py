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

"""Shell completion — pylon completion <shell>."""

from __future__ import annotations

import click
from click.shell_completion import get_completion_class

_SHELLS = ("bash", "zsh", "fish")

_INSTALL_HINT = """\
Usage: eval "$(pylon completion SHELL)"

Add one of these to your shell's rc file, then restart the shell (or source it):

  bash:  eval "$(pylon completion bash)"   >> ~/.bashrc
  zsh:   eval "$(pylon completion zsh)"    >> ~/.zshrc
  fish:  pylon completion fish | source    >> ~/.config/fish/config.fish
"""


@click.command("completion")
@click.argument("shell", required=False, type=click.Choice(_SHELLS))
@click.pass_context
def completion_cmd(ctx: click.Context, shell: str | None) -> None:
    """Print a shell completion script.

    Run with no argument for setup instructions.
    """
    if shell is None:
        click.echo(_INSTALL_HINT)
        return

    root_ctx = ctx.find_root()
    complete_var = "_PYLON_COMPLETE"
    comp_cls = get_completion_class(shell)
    comp = comp_cls(root_ctx.command, {}, "pylon", complete_var)
    click.echo(comp.source())
