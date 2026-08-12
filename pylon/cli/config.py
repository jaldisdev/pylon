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

import functools

import click

from .banner import _BOLD_RED, _INFO_COLOR, _RESET


def _print_error(message: str, hint: str) -> None:
    click.echo(f'{_BOLD_RED}error:{_RESET} {message}', err=True)
    click.echo(f'{_INFO_COLOR}Hint: {hint}{_RESET}', err=True)


NO_CONFIG_HINT = 'Create a pylon.toml in your project root or run this command from within a Pylon project directory.'


def requires_config(fn):
    """Decorator for commands that require a loaded pylon.toml.

    Reads the config from the Click context object. If no config was found,
    prints the banner and an error before exiting.
    """

    @functools.wraps(fn)
    @click.pass_context
    def wrapper(ctx: click.Context, *args, **kwargs):
        if ctx.obj.get('config') is None:
            _print_error('no pylon.toml found', NO_CONFIG_HINT)
            ctx.exit(1)
        return ctx.invoke(fn, *args, **kwargs)

    return wrapper
