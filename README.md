# basal

The flow engine for CortexKit. basal runs small scripts, called flows, that react to events or to a schedule, read facts and module data, decide, and deliver what an agent should hear about into that agent's sinks. It also serves codemode: the same runtime, used live by an agent inside a session.

The name comes from the basal ganglia, the part of the brain that selects actions and runs habits and routines.

basal will run as a module supervised by the subc daemon, with the binary `ck-basal`. Nothing is built yet. The design is in [docs/design.md](docs/design.md), and a new session starts with [docs/onboarding.md](docs/onboarding.md).
