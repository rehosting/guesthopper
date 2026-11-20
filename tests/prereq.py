"""Prerequisite handling for tests that need more than the standard library.

Some suites need asyncssh, OpenSSH's clients, or the built ``uds_agent``
example. On a developer machine a missing one should just skip the suite. In
CI that is exactly the wrong outcome: the job would go green without having
run the tests it exists to run. Setting ``GUESTHOPPER_REQUIRE_ALL_TESTS=1``
(tests/run_all.sh does) turns a missing prerequisite into a failure instead.
"""

import os
import unittest

REQUIRE_ALL = os.environ.get("GUESTHOPPER_REQUIRE_ALL_TESTS", "") not in ("", "0")


def requires(**prerequisites):
    """Class decorator: ``requires(asyncssh=have_it, scp="hint if missing")``.

    Each keyword names a prerequisite; its value is truthy when present. A
    value may be a ``(present, hint)`` pair to say how to provide it.
    """
    missing = []
    for name, value in prerequisites.items():
        present, hint = value if isinstance(value, tuple) else (value, "")
        if not present:
            missing.append(f"{name} ({hint})" if hint else name)

    def decorate(cls):
        if not missing:
            return cls
        reason = "missing prerequisite(s): " + ", ".join(missing)
        if not REQUIRE_ALL:
            return unittest.skip(reason)(cls)

        def fail(_cls):
            raise AssertionError(
                f"{reason} -- and GUESTHOPPER_REQUIRE_ALL_TESTS is set, so "
                f"{cls.__name__} may not be skipped")

        cls.setUpClass = classmethod(fail)
        return cls

    return decorate
