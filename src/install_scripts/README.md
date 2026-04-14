These files are downloaded by release.sh at release time and committed as part
of the version bump. They are embedded in the host-tools binary via include_bytes!.

To build host-tools locally, run:
  curl -fsSL https://claude.ai/install.sh   -o src/install_scripts/claude_install.sh
  curl -fsSL https://opencode.ai/install.sh -o src/install_scripts/opencode_install.sh
