# Repository guide

Use the `ligmaSec/victory-royale` repository for issues and changes.

Follow the [code standards](docs/code-standards.md) and [asset policy](docs/asset-policy.md).
The [development guide](CONTRIBUTING.md) covers setup and checks. See the [architecture guide](docs/architecture.md)
for crate responsibilities, terminology, and the import-resolution order.

For title-specific compatibility work, check the affected behavior against the
user-supplied game build and the relevant sections of its platform specification
when available. Record the build, the observation, and what still needs a runtime
or hardware check. Keep game material and derived files outside the checkout.
Use synthetic inputs for committed regression tests.
