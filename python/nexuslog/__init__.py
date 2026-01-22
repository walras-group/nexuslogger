"""Fast async logger with Rust backend.

Usage:
    import nexuslog as logging
    logging.basicConfig(level=logging.INFO)
    logging.info("Hello, world!")
"""

from ._logger import (
    PyLevel as Level,
    PyLogger as _PyLogger,
    get_logger as _get_logger,
    basic_config as _basic_config,
)

# Standard logging level constants
TRACE = Level.Trace
DEBUG = Level.Debug
INFO = Level.Info
WARNING = Level.Warn
ERROR = Level.Error

__all__ = [
    "Level",
    "Logger",
    "basicConfig",
    "getLogger",
    "TRACE",
    "DEBUG",
    "INFO",
    "WARNING",
    "ERROR",
    "trace",
    "debug",
    "info",
    "warning",
    "error",
    "shutdown",
]

_DEFAULT_LEVEL = INFO
_root_logger: "Logger | None" = None


def basicConfig(
    filename: str | None = None, level: Level = INFO, unix_ts: bool = False
) -> None:
    """Configure the root logger.

    Args:
        filename: Optional file path for log output. If None, logs to stdout.
        level: Minimum log level to record. Default is INFO.
        unix_ts: If True, emit unix timestamps instead of formatted local time.
    """
    global _DEFAULT_LEVEL, _root_logger
    _basic_config(filename, unix_ts)
    _DEFAULT_LEVEL = level
    # Create root logger
    _root_logger = getLogger(None, level)


class Logger:
    """Fast async logger instance.

    Args:
        name: Logger name. If None, the name field is omitted in log output.
        path: Optional file path prefix for log files. If None, logs to stdout.
              Log files are rotated daily with format: {path}_YYYYMMDD.log
        level: Minimum log level to record. Default is Level.Info.
    """

    def __init__(
        self, name: str | None, path: str | None = None, level: Level = Level.Info
    ) -> None:
        self._logger = _PyLogger(name, path, level)

    def shutdown(self) -> None:
        """Shutdown the logger and flush remaining messages."""
        self._logger.shutdown()

    def trace(self, message: str) -> None:
        """Log a trace message."""
        self._logger.trace(message)

    def debug(self, message: str) -> None:
        """Log a debug message."""
        self._logger.debug(message)

    def info(self, message: str) -> None:
        """Log an info message."""
        self._logger.info(message)

    def warning(self, message: str) -> None:
        """Log a warning message."""
        self._logger.warn(message)

    def error(self, message: str) -> None:
        """Log an error message."""
        self._logger.error(message)


def getLogger(name: str | None = None, level: Level | None = None) -> Logger:
    """Get a logger that shares a writer with the default path."""
    if level is None:
        level = _DEFAULT_LEVEL
    logger = Logger.__new__(Logger)
    logger._logger = _get_logger(name, level)
    return logger


def _get_root_logger() -> Logger:
    """Get or create the root logger."""
    global _root_logger
    if _root_logger is None:
        _root_logger = getLogger(None, _DEFAULT_LEVEL)
    return _root_logger


def trace(message: str) -> None:
    """Log a trace message to the root logger."""
    _get_root_logger().trace(message)


def debug(message: str) -> None:
    """Log a debug message to the root logger."""
    _get_root_logger().debug(message)


def info(message: str) -> None:
    """Log an info message to the root logger."""
    _get_root_logger().info(message)


def warning(message: str) -> None:
    """Log a warning message to the root logger."""
    _get_root_logger().warning(message)


def error(message: str) -> None:
    """Log an error message to the root logger."""
    _get_root_logger().error(message)


def shutdown() -> None:
    """Shutdown the root logger and flush remaining messages."""
    _get_root_logger().shutdown()
