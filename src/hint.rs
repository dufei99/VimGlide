//! 提示标签的生成与匹配状态机（与平台无关，可单独单元测试）。

use std::collections::HashMap;

use crate::model::Target;

/// 一次按键的处理结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// 缓冲区变化，需要重绘。
    Updated,
    /// 唯一确定了一个目标。
    Selected(usize),
    /// 没有任何标签以当前输入为前缀，输入已被丢弃。
    Invalid,
}

/// 一组提示标签及其输入缓冲区。
pub struct HintSet {
    targets: Vec<Target>,
    labels: Vec<String>,
    buffer: String,
    /// 已确定的目标；选中后等待用户按空格执行动作。
    selected: Option<usize>,
}

impl HintSet {
    pub fn new(targets: Vec<Target>, alphabet: &str) -> Self {
        let alphabet = sanitize_alphabet(alphabet);
        let labels = generate_labels(&targets, &alphabet);
        Self {
            targets,
            labels,
            buffer: String::new(),
            selected: None,
        }
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn target(&self, index: usize) -> &Target {
        &self.targets[index]
    }

    /// 当前标签是否与已输入内容匹配。
    pub fn is_matched(&self, index: usize) -> bool {
        self.labels[index].starts_with(&self.buffer)
    }

    pub fn matched_count(&self) -> usize {
        (0..self.labels.len())
            .filter(|&i| self.is_matched(i))
            .count()
    }

    /// 输入一个字符（小写）。
    pub fn press(&mut self, c: char) -> Press {
        self.buffer.push(c);

        let mut exact = None;
        let mut has_longer = false;
        for (i, label) in self.labels.iter().enumerate() {
            if label.starts_with(&self.buffer) {
                if label.len() == self.buffer.len() {
                    exact = Some(i);
                } else {
                    has_longer = true;
                }
            }
        }

        match (exact, has_longer) {
            // 标签集合保证「没有标签是另一个标签的前缀」，因此这里选中是安全的。
            (Some(i), false) => Press::Selected(i),
            (Some(_), true) | (None, true) => Press::Updated,
            (None, false) => {
                self.buffer.pop();
                Press::Invalid
            }
        }
    }

    /// 退格。
    pub fn backspace(&mut self) {
        self.buffer.pop();
        self.selected = None;
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.selected = None;
    }

    /// 标记选中的目标：标签已完整输入，等待用户确认动作。
    pub fn select(&mut self, index: usize) {
        self.selected = Some(index);
    }

    /// 当前选中的目标。
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }
}

/// 清洗提示字符表：去重、只保留小写 ASCII 字母/数字。
fn sanitize_alphabet(alphabet: &str) -> Vec<char> {
    let mut chars: Vec<char> = Vec::new();
    for c in alphabet.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() && !chars.contains(&c) {
            chars.push(c);
        }
    }
    if chars.is_empty() {
        chars = "sadfjklewcmpgh".chars().collect();
    }
    chars
}

/// 生成标签，规则如下：
/// 1. 优先用元素名称的首字母作为单字符助记标签（仅当该首字母唯一时）；
/// 2. 剩余元素不足以占用不同单字符时，改用「首字符不与单字符标签冲突」的双字符标签；
/// 3. 标签数量不够时整体退化为等长三字符标签。
/// 这样能保证任何标签都不是其它标签的前缀。
fn generate_labels(targets: &[Target], alphabet: &[char]) -> Vec<String> {
    let n = targets.len();
    let mut labels: Vec<Option<String>> = vec![None; n];

    // 统计助记首字母的出现次数。
    let mnemonic: Vec<Option<char>> = targets
        .iter()
        .map(|t| mnemonic_of(&t.name, alphabet))
        .collect();
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in mnemonic.iter().flatten() {
        *counts.entry(*c).or_insert(0) += 1;
    }

    let mut single_chars: Vec<char> = Vec::new();
    for (i, c) in mnemonic.iter().enumerate() {
        if let Some(c) = c {
            if counts.get(c).copied() == Some(1) {
                labels[i] = Some(c.to_string());
                single_chars.push(*c);
            }
        }
    }
    single_chars.sort_unstable();

    // 双字符标签：首字符避开单字符标签，保证前缀唯一。
    let remaining = labels.iter().filter(|l| l.is_none()).count();
    if remaining > 0 {
        let free_singles: Vec<char> = alphabet
            .iter()
            .copied()
            .filter(|c| !single_chars.contains(c))
            .collect();

        // 剩余元素能用不同单字符区分时，优先用单字符（如少量中文名称的按钮）。
        if remaining <= free_singles.len() {
            let mut singles = free_singles.into_iter();
            for slot in labels.iter_mut() {
                if slot.is_none() {
                    *slot = singles.next().map(|c| c.to_string());
                }
            }
        } else {
            let mut pairs = free_singles
                .iter()
                .flat_map(|a| alphabet.iter().map(move |b| format!("{a}{b}")));

            let capacity = free_singles.len() * alphabet.len();
            if remaining <= capacity {
                for slot in labels.iter_mut() {
                    if slot.is_none() {
                        *slot = pairs.next();
                    }
                }
            } else {
                // 极端情况（元素过多）：全部改为 3 字符标签。
                let mut triples = alphabet.iter().flat_map(|a| {
                    alphabet
                        .iter()
                        .flat_map(move |b| alphabet.iter().map(move |c| format!("{a}{b}{c}")))
                });
                labels = (0..n).map(|_| triples.next()).collect();
            }
        }
    }

    labels
        .into_iter()
        .map(|l| l.unwrap_or_default())
        .collect()
}

/// 从元素名称中取第一个属于提示字符表的字母（跳过空白与符号）。
fn mnemonic_of(name: &str, alphabet: &[char]) -> Option<char> {
    name.chars()
        .flat_map(|c| c.to_lowercase())
        .find(|c| alphabet.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rect;

    fn target(name: &str, x: i32, y: i32) -> Target {
        Target {
            rect: Rect::new(x, y, 40, 20),
            name: name.to_string(),
        }
    }

    #[test]
    fn mnemonic_labels_are_used_when_unique() {
        let hints = HintSet::new(
            vec![
                target("Save File", 0, 0),
                target("Close Tab", 0, 40),
                target("Add Item", 0, 80),
            ],
            "sadfjklewcmpgh",
        );
        assert_eq!(hints.labels(), &["s", "c", "a"]);
    }

    #[test]
    fn duplicate_mnemonics_fall_back_to_pairs() {
        let hints = HintSet::new(
            vec![target("Open", 0, 0), target("Open", 0, 40), target("Save", 0, 80)],
            "sd",
        );
        assert_eq!(hints.labels(), &["ds", "dd", "s"]);
    }

    #[test]
    fn no_label_is_prefix_of_another() {
        let targets = (0..50)
            .map(|i| target("", i * 10, i * 10))
            .collect::<Vec<_>>();
        let hints = HintSet::new(targets, "asdf");
        for a in hints.labels() {
            for b in hints.labels() {
                if a != b {
                    assert!(!b.starts_with(a.as_str()), "{a} 是 {b} 的前缀");
                }
            }
        }
    }

    #[test]
    fn typing_selects_exact_label() {
        let mut hints = HintSet::new(
            vec![target("Open", 0, 0), target("", 0, 40)],
            "osd",
        );
        assert_eq!(hints.labels(), &["o", "s"]);
        assert_eq!(hints.press('o'), Press::Selected(0));
    }

    #[test]
    fn few_targets_without_mnemonic_use_single_char() {
        let hints = HintSet::new(
            vec![
                target("最小化", 0, 0),
                target("最大化", 0, 40),
                target("关闭", 0, 80),
            ],
            "sadfjklewcmpgh",
        );
        assert_eq!(hints.labels(), &["s", "a", "d"]);
    }

    #[test]
    fn invalid_input_is_dropped() {
        let mut hints = HintSet::new(vec![target("Open", 0, 0)], "sd");
        assert_eq!(hints.press('x'), Press::Invalid);
        assert_eq!(hints.buffer(), "");
    }
}
