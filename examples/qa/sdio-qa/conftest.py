"""Options of the SDIO hardware recipe. Collection never touches a board or the network."""


def pytest_addoption(parser):
    group = parser.getgroup("sdio-qa")
    group.addoption("--sdio-qa-params", help="Private JSON parameters (see parameters.example.json)")
    group.addoption("--sdio-qa-consent", help="Private JSON consent written by the operator")
    group.addoption("--sdio-qa-interactive", action="store_true",
                    help="Wait for the operator after each physical action (run pytest with -s)")
