import {
  Bot,
  FilePen,
  FileText,
  Globe,
  List,
  ListChecks,
  Search,
  SquareTerminal,
  Wrench,
  type LucideIcon,
} from 'lucide-react-native';

/**
 * A lucide icon for a tool call from its name alone. The fold carries no icon, so
 * this is a client-side match — lower-cased substring tests, so provider-prefixed
 * names (`mcp__server__read_file`) still resolve — with a generic wrench fallback.
 * Purely presentational: an unrecognised tool still renders, just with the default
 * glyph, so new tools never need a code change to look right.
 */
export function toolIcon(name: string): LucideIcon {
  const n = name.toLowerCase();
  if (/(bash|shell|terminal|exec|command)/.test(n)) return SquareTerminal;
  if (/(edit|write|patch|replace|create|apply)/.test(n)) return FilePen;
  if (/todo/.test(n)) return ListChecks;
  if (/(agent|subagent|dispatch|spawn|task)/.test(n)) return Bot;
  if (/(grep|search|glob|find|ripgrep)/.test(n)) return Search;
  if (/(fetch|http|web|url|browser|curl)/.test(n)) return Globe;
  if (/(read|view|open|cat|file)/.test(n)) return FileText;
  if (/(\bls\b|list|tree|dir)/.test(n)) return List;
  return Wrench;
}
