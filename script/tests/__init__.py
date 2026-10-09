"""Discover the offline checks, whose module names describe their contracts."""


def load_tests(loader, tests, pattern):
    # These suites predate unittest's test_*.py naming convention. Keep their
    # import paths stable for mutation controls while making discovery run them.
    return loader.loadTestsFromNames([
        "script.tests.check_path_deps",
        "script.tests.check_ckdev_names",
        "script.tests.flows_rig",
        "script.tests.sign_worker",
        "script.tests.stage_card",
    ])
