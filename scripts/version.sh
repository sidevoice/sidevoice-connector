#!/bin/sh
# The product version. A release is a git tag v<version>, and these manifests
# are what that tag publishes under: the npm client and the server package.
# This script is the only thing that writes them, and CI checks they agree with
# the tag before anything is published. See docs/RELEASING.md.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
package="$root/packages/connector/package.json"
pyproject="$root/apps/server/pyproject.toml"

usage() {
 echo "usage: scripts/version.sh show | check <version> | set <version>" >&2
 exit 2
}

package_version() { node -p "require('$package').version"; }
pyproject_version() { sed -n 's/^version = "\(.*\)"$/\1/p' "$pyproject" | head -1; }

[ $# -ge 1 ] || usage
action=$1
version=${2:-}

case "$action" in
 show)
  echo "packages/connector/package.json $(package_version)"
  echo "apps/server/pyproject.toml      $(pyproject_version)"
  ;;
 check)
  [ -n "$version" ] || usage
  status=0
  for pair in "packages/connector/package.json:$(package_version)" "apps/server/pyproject.toml:$(pyproject_version)"; do
   file=${pair%%:*}
   found=${pair#*:}
   if [ "$found" != "$version" ]; then
    echo "$file says $found, expected $version. Run scripts/version.sh set $version." >&2
    status=1
   fi
  done
  if [ "$status" -eq 0 ]; then
   echo "Every manifest says $version."
  fi
  exit "$status"
  ;;
 set)
  case "$version" in
   [0-9]*.[0-9]*.[0-9]*) ;;
   *) echo "A version looks like 1.2.3 or 1.2.3-rc.1, not '$version'." >&2; exit 2 ;;
  esac
  node -e '
   const fs = require("fs");
   const [file, version] = process.argv.slice(1);
   const manifest = JSON.parse(fs.readFileSync(file, "utf8"));
   manifest.version = version;
   fs.writeFileSync(file, JSON.stringify(manifest, null, 2) + "\n");
  ' "$package" "$version"
  sed -i.bak "s/^version = \".*\"$/version = \"$version\"/" "$pyproject" && rm -f "$pyproject.bak"
  # The lock file records the workspace's version too, and npm ci refuses to
  # run while it disagrees with the manifest.
  (cd "$root" && npm install --package-lock-only --silent)
  echo "Set $version. Commit the manifests and the lock file, then tag v$version."
  ;;
 *) usage ;;
esac
