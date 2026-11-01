import functools


def decorate(fn):
    @functools.wraps(fn)
    def inner(*args, **kwargs):
        return fn(*args, **kwargs)
    return inner


class Engine:
    """Runs things."""

    def __init__(self, name: str) -> None:
        self.name = name

    @decorate
    def run(self, times: int = 1) -> str:
        return helper(self.name) * times


def helper(value: str) -> str:
    def nested() -> str:
        return value
    return nested()


def çalıştır() -> None:
    Engine("x").run()
