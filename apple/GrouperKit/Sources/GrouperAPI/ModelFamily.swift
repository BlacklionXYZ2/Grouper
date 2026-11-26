import Foundation

/// A model's family, so the apps can give it a colour accent and a short glyph.
///
/// Ported from the web client's `lib/family.js`. The family comes from the
/// registry entry's provider model string (then its id), matched on clear name
/// prefixes only. Anything else is `.unknown`: never a guess. The colour is a
/// supplement; the model's name is always shown as text beside it.
public enum ModelFamily: String, CaseIterable, Equatable, Hashable, Sendable {
    case claude, gpt, gemini, llama, qwen, deepseek, mistral, nemotron, ernie, grok, radium, scripted, unknown

    /// The family name for people, e.g. "Claude".
    public var label: String {
        switch self {
        case .claude: return "Claude"
        case .gpt: return "GPT"
        case .gemini: return "Gemini"
        case .llama: return "Llama"
        case .qwen: return "Qwen"
        case .deepseek: return "DeepSeek"
        case .mistral: return "Mistral"
        case .nemotron: return "Nemotron"
        case .ernie: return "ERNIE"
        case .grok: return "Grok"
        case .radium: return "Radium"
        case .scripted: return "Scripted"
        case .unknown: return "Unknown family"
        }
    }

    public var maker: String {
        switch self {
        case .claude: return "Anthropic"
        case .gpt: return "OpenAI"
        case .gemini: return "Google"
        case .llama: return "Meta"
        case .qwen: return "Alibaba"
        case .deepseek: return "DeepSeek"
        case .mistral: return "Mistral AI"
        case .nemotron: return "NVIDIA"
        case .ernie: return "Baidu"
        case .grok: return "xAI"
        case .radium: return "Radium"
        case .scripted: return "test"
        case .unknown: return ""
        }
    }

    /// The short non-colour cue. Empty for `.unknown` (the apps show a generic model icon).
    public var glyph: String {
        switch self {
        case .claude: return "Cl"
        case .gpt: return "GP"
        case .gemini: return "Ge"
        case .llama: return "Ll"
        case .qwen: return "Qw"
        case .deepseek: return "DS"
        case .mistral: return "Mi"
        case .nemotron: return "Nv"
        case .ernie: return "Er"
        case .grok: return "Gk"
        case .radium: return "Ra"
        case .scripted: return "T"
        case .unknown: return ""
        }
    }

    /// Words for screen readers, e.g. "Claude family" or "family unknown".
    public var accessibilityLabel: String {
        self == .unknown ? "family unknown" : "\(label) family"
    }

    /// The family of a registry entry: its provider model string wins, its id is the fallback.
    public static func of(_ entry: ModelEntry) -> ModelFamily {
        match(entry.model) ?? match(entry.id) ?? .unknown
    }

    /// The family of a bare model string.
    public static func of(model: String) -> ModelFamily {
        match(model) ?? .unknown
    }

    /// The family of a registry id as the hub reports it on a thread (`ThreadInfo.model`).
    public static func of(registryId id: String, in models: [ModelEntry]) -> ModelFamily {
        if let entry = models.first(where: { $0.id == id }) { return of(entry) }
        return of(model: id)
    }

    // MARK: - Matching (the rules of lib/family.js, without regular expressions)

    /// The model's own name: the part after the last "/", lower-cased, without a
    /// ":free" / ":beta" style variant suffix.
    static func nameOf(_ raw: String) -> String {
        let lowered = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        var name = lowered.split(separator: "/", omittingEmptySubsequences: false).last.map(String.init) ?? ""
        if let colon = name.lastIndex(of: ":") {
            let suffix = name[name.index(after: colon)...]
            if !suffix.isEmpty, suffix.allSatisfy({ $0 == "-" || ("a"..."z").contains($0) || ("0"..."9").contains($0) }) {
                name = String(name[..<colon])
            }
        }
        return name
    }

    static func match(_ raw: String) -> ModelFamily? {
        let name = nameOf(raw)
        guard !name.isEmpty else { return nil }
        // Order matters only where one name holds another family's word: Nemotron
        // models built on Llama say "nemotron" explicitly, so that is checked first.
        if starts(name, "nemotron", digits: true) || hasInnerWord(name, "-nemotron") { return .nemotron }
        if starts(name, "claude", digits: true) || isBedrockClaude(name) { return .claude }
        if starts(name, "gpt", digits: true) || starts(name, "chatgpt", digits: false) || isOSeries(name) { return .gpt }
        if starts(name, "gemini", digits: true) || starts(name, "gemma", digits: true) { return .gemini }
        if starts(name, "llama", digits: true) || starts(name, "meta-llama", digits: false) || starts(name, "codellama", digits: false) { return .llama }
        if starts(name, "qwen", digits: true) || starts(name, "qwq", digits: true) { return .qwen }
        if starts(name, "deepseek", digits: false) { return .deepseek }
        if ["mistral", "mixtral", "codestral", "ministral", "magistral", "devstral", "pixtral"].contains(where: { starts(name, $0, digits: false) }) { return .mistral }
        if starts(name, "ernie", digits: true) { return .ernie }
        if starts(name, "grok", digits: true) { return .grok }
        if ["tycho", "hal", "clarke"].contains(where: { starts(name, $0, digits: true) }) { return .radium }
        if ["fake", "scripted"].contains(where: { starts(name, $0, digits: true) }) { return .scripted }
        return nil
    }

    private static func isSeparator(_ c: Character, digits: Bool) -> Bool {
        c == "-" || c == "_" || c == "." || (digits && ("0"..."9").contains(c))
    }

    /// `^prefix(?:[-_.]|$)`, or `^prefix(?:[-_.\d]|$)` when `digits`.
    private static func starts(_ name: String, _ prefix: String, digits: Bool) -> Bool {
        guard name.hasPrefix(prefix) else { return false }
        guard let next = name.dropFirst(prefix.count).first else { return true }
        return isSeparator(next, digits: digits)
    }

    /// `word(?:[-_.]|$)` anywhere in the name.
    private static func hasInnerWord(_ name: String, _ word: String) -> Bool {
        let parts = name.components(separatedBy: word)
        guard parts.count > 1 else { return false }
        return parts.dropFirst().contains { part in
            guard let next = part.first else { return true }
            return isSeparator(next, digits: false)
        }
    }

    /// `^(?:(?:us|eu|apac|global)\.)?anthropic\.claude`
    private static func isBedrockClaude(_ name: String) -> Bool {
        for region in ["", "us.", "eu.", "apac.", "global."] where name.hasPrefix(region + "anthropic.claude") { return true }
        return false
    }

    /// `^o[134](?:-|$)`
    private static func isOSeries(_ name: String) -> Bool {
        let chars = Array(name)
        guard chars.count >= 2, chars[0] == "o", "134".contains(chars[1]) else { return false }
        return chars.count == 2 || chars[2] == "-"
    }
}
