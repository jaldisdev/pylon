import functools

import click

from .banner import _BOLD_RED, _INFO_COLOR, _RESET, print_banner


def _print_error(message: str, hint: str) -> None:
    print_banner()
    click.echo(f"{_BOLD_RED}error:{_RESET} {message}", err=True)
    click.echo(f"{_INFO_COLOR}Hint: {hint}{_RESET}", err=True)


NO_CONFIG_HINT = (
    "Create a pylon.toml in your project root or run this command "
    "from within a Pylon project directory."
)


def requires_config(fn):
    """Decorator for commands that require a loaded pylon.toml.

    Reads the config from the Click context object. If no config was found,
    prints the banner and an error before exiting.
    """

    @functools.wraps(fn)
    @click.pass_context
    def wrapper(ctx: click.Context, *args, **kwargs):
        if ctx.obj.get("config") is None:
            _print_error("no pylon.toml found", NO_CONFIG_HINT)
            ctx.exit(1)
        return ctx.invoke(fn, *args, **kwargs)

    return wrapper
