import { page, group } from '../scripts/docs.mjs'

// Curated reading order. Add every new document here: docs.test.mjs fails otherwise.
export const sidebar = [
  page('README', 'Documentation home'),
  group('Start here', [
    page('guides/installation/from-source'),
    page('tutorials/first-configuration'),
    page('tutorials/first-agent'),
  ], false),
  group('Concepts', [
    page('explanation/providers-models-profiles-agents'),
    page('architecture', 'Architecture'),
    page('invariants'),
  ], false),
  group('Guides', [
    group('Configuration', [
      page('guides/configuration/init-lua'),
      page('guides/system-prompt'),
      page('guides/configuration/colorschemes'),
      page('guides/configuration/tui-commands'),
    ]),
    group('Plugins', [
      page('guides/plugins/loading-and-lifecycle'),
      page('guides/plugins/packages'),
      page('guides/plugins/example-recipes'),
      page('session-search'),
    ]),
    group('Agents and delegation', [
      page('guides/subagent-commands'),
      page('guides/workflows', 'Workflows'),
      page('guides/background-jobs'),
      page('guides/queue-and-steer'),
      page('guides/scheduled-reminders'),
    ]),
    group('Tools', [
      page('guides/terminals'),
      page('guides/image-reading'),
    ]),
    group('Integration and safety', [
      page('guides/execution-hardening'),
      page('guides/control-socket'),
    ]),
  ], false),
  group('Reference', [
    group('Configuration', [
      page('reference/configuration-precedence'),
      page('reference/configuration/providers'),
      page('reference/configuration/profiles'),
      page('reference/configuration/agents'),
      page('reference/configuration/sandbox'),
    ]),
    group('Tools', [
      page('reference/tools/subagent'),
      page('reference/tools/terminal'),
      page('reference/tools/workflow'),
    ]),
    group('Lua API', [
      page('reference/lua/commands'),
      page('reference/lua/subagents'),
      page('reference/lua/task'),
      page('reference/lua/timer'),
      page('reference/lua/json'),
      page('reference/lua/budgets'),
    ]),
    page('reference/http-api'),
  ]),
  group('Project', [
    page('project/known-limitations'),
    page('contributing/README', 'Contributing'),
    page('contributing/documentation'),
    page('contributing/native-extensions'),
  ]),
]
