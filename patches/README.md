# Ghostty Linux embedded patch

`ghostty-linux-embedded.patch` reapplies the Linux embedding and display
lifecycle changes from the former Ghostty fork to upstream v1.3.1. It is based
on [2e86ff096](https://github.com/am-will/ghostty/commit/2e86ff096) and
[cee28ebfe](https://github.com/am-will/ghostty/commit/cee28ebfe). Build through
`scripts/build-ghostty.sh` so the submodule stays clean.
