#!/bin/sh
# The product version. A release is a git tag v<version>, and this manifest is what that tag publishes
# under: the npm client. This script is the only thing that writes it, and CI checks it agrees with the
# tag before anything is published.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
package="$root/packages/connector/package.json"

usage() {
 echo "usage: scripts/version.sh show | check <version> | set <version>" >&2
 exit 2
}

package_version() { node -p "require('$package').version"; }

[ $# -ge 1 ] || usage
action=$1
version=${2:-}

case "$action" in
 show)
  echo "packages/connector/package.json $(package_version)"
  ;;
 check)
  [ -n "$version" ] || usage
  found=$(package_version)
  if [ "$found" != "$version" ]; then
   echo "packages/connector/package.json says $found, expected $version. Run scripts/version.sh set $version." >&2
   exit 1
  fi
  echo "Every manifest says $version."
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
  # The lock file records the workspace's version too, and npm ci refuses to
  # run while it disagrees with the manifest.
  (cd "$root" && npm install --package-lock-only --silent)
  echo "Set $version. Commit the manifest and the lock file, then tag v$version."
  ;;
 *) usage ;;
esac
