// Managed by Reef. Install with: reef agents setup opencode
import { existsSync } from "node:fs"

export default {
  id: "reef.observe",
  setup() {
    const paths = [process.env.REEF_BIN]
    if (process.platform === "darwin") {
      paths.push("/opt/homebrew/bin/reef", "/usr/local/bin/reef")
    }
    const reef = paths.find((path) => path && existsSync(path)) ?? "reef"
    try {
      const args = [reef, "agents", "observe", "opencode", "--pid", String(process.pid)]
      if (process.env.REEF_OBSERVE_STATE_DIR) {
        args.push("--state-dir", process.env.REEF_OBSERVE_STATE_DIR)
      }
      const child = Bun.spawn(args, { stdin: "ignore", stdout: "ignore", stderr: "ignore" })
      child.unref()
    } catch {
      // Missing Reef must not change an OpenCode session outcome.
    }
  },
}
