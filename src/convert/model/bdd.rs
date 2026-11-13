//! Conditions as reduced ordered binary decision diagrams: deciding whether one can hold, or always holds, takes time
//! in the size of its diagram rather than 2^names, and nothing here recurses deeper than the number of names.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

pub type Node = u32;
pub const FALSE: Node = 0;
pub const TRUE: Node = 1;

// The variable of the two terminal nodes: below every real one
const TERMINAL: u32 = u32::MAX;

#[derive(Default)]
struct Manager {
    // (variable, low, high) per node
    nodes: Vec<(u32, Node, Node)>,
    unique: HashMap<(u32, Node, Node), Node>,
    and_cache: HashMap<(Node, Node), Node>,
    not_cache: HashMap<Node, Node>,
    variables: HashMap<String, u32>,
    names: Vec<String>,
}

thread_local! {
    static MANAGER: RefCell<Manager> = RefCell::new(Manager::new());
}

impl Manager {
    fn new() -> Manager {
        let mut manager = Manager::default();
        manager.nodes.push((TERMINAL, FALSE, FALSE));
        manager.nodes.push((TERMINAL, TRUE, TRUE));
        manager
    }

    fn make(&mut self, variable: u32, low: Node, high: Node) -> Node {
        if low == high {
            return low;
        }
        if let Some(node) = self.unique.get(&(variable, low, high)) {
            return *node;
        }
        let node = self.nodes.len() as Node;
        self.nodes.push((variable, low, high));
        self.unique.insert((variable, low, high), node);
        node
    }

    fn variable(&mut self, name: &str) -> Node {
        let index = match self.variables.get(name) {
            Some(index) => *index,
            None => {
                let index = self.names.len() as u32;
                self.names.push(name.to_string());
                self.variables.insert(name.to_string(), index);
                index
            }
        };
        self.make(index, FALSE, TRUE)
    }

    fn not(&mut self, node: Node) -> Node {
        match node {
            FALSE => return TRUE,
            TRUE => return FALSE,
            _ => {}
        }
        if let Some(result) = self.not_cache.get(&node) {
            return *result;
        }
        let (variable, low, high) = self.nodes[node as usize];
        let (low, high) = (self.not(low), self.not(high));
        let result = self.make(variable, low, high);
        self.not_cache.insert(node, result);
        result
    }

    fn and(&mut self, left: Node, right: Node) -> Node {
        if left == FALSE || right == FALSE {
            return FALSE;
        }
        if left == TRUE || left == right {
            return right;
        }
        if right == TRUE {
            return left;
        }
        let key = (left.min(right), left.max(right));
        if let Some(result) = self.and_cache.get(&key) {
            return *result;
        }

        let (variable, (left_low, left_high), (right_low, right_high)) = self.split(left, right);
        let low = self.and(left_low, right_low);
        let high = self.and(left_high, right_high);
        let result = self.make(variable, low, high);
        self.and_cache.insert(key, result);
        result
    }

    fn or(&mut self, left: Node, right: Node) -> Node {
        let (left, right) = (self.not(left), self.not(right));
        let both = self.and(left, right);
        self.not(both)
    }

    /// The top variable of two nodes and each one's cofactors on it.
    fn split(&self, left: Node, right: Node) -> (u32, (Node, Node), (Node, Node)) {
        let variable = self.nodes[left as usize].0.min(self.nodes[right as usize].0);
        (variable, self.cofactors(left, variable), self.cofactors(right, variable))
    }

    fn cofactors(&self, node: Node, variable: u32) -> (Node, Node) {
        let (top, low, high) = self.nodes[node as usize];
        if top == variable {
            (low, high)
        } else {
            (node, node)
        }
    }

    /// Minato-Morreale: an irredundant sum of products between `lower` and `upper`, with the node it stands for.
    fn isop(&mut self, lower: Node, upper: Node, cache: &mut HashMap<(Node, Node), (Vec<Vec<(u32, bool)>>, Node)>) -> (Vec<Vec<(u32, bool)>>, Node) {
        if lower == FALSE {
            return (Vec::new(), FALSE);
        }
        if upper == TRUE {
            return (vec![Vec::new()], TRUE);
        }
        if let Some(result) = cache.get(&(lower, upper)) {
            return result.clone();
        }

        let (variable, (lower_low, lower_high), (upper_low, upper_high)) = self.split(lower, upper);
        let not_upper_high = self.not(upper_high);
        let only_low = self.and(lower_low, not_upper_high);
        let (low_cubes, low_node) = self.isop(only_low, upper_low, cache);
        let not_upper_low = self.not(upper_low);
        let only_high = self.and(lower_high, not_upper_low);
        let (high_cubes, high_node) = self.isop(only_high, upper_high, cache);

        let not_low_node = self.not(low_node);
        let not_high_node = self.not(high_node);
        let rest_low = self.and(lower_low, not_low_node);
        let rest_high = self.and(lower_high, not_high_node);
        let rest = self.or(rest_low, rest_high);
        let both_upper = self.and(upper_low, upper_high);
        let (shared_cubes, shared_node) = self.isop(rest, both_upper, cache);

        let literal = self.make(variable, FALSE, TRUE);
        let not_literal = self.not(literal);
        let low_part = self.and(not_literal, low_node);
        let high_part = self.and(literal, high_node);
        let node = self.or(low_part, high_part);
        let node = self.or(node, shared_node);

        let mut cubes = Vec::new();
        cubes.extend(low_cubes.into_iter().map(|mut cube| {
            cube.push((variable, false));
            cube
        }));
        cubes.extend(high_cubes.into_iter().map(|mut cube| {
            cube.push((variable, true));
            cube
        }));
        cubes.extend(shared_cubes);

        cache.insert((lower, upper), (cubes.clone(), node));
        (cubes, node)
    }
}

fn with<T>(work: impl FnOnce(&mut Manager) -> T) -> T {
    MANAGER.with(|manager| work(&mut manager.borrow_mut()))
}

pub fn variable(name: &str) -> Node {
    with(|manager| manager.variable(name))
}

pub fn and(left: Node, right: Node) -> Node {
    with(|manager| manager.and(left, right))
}

pub fn or(left: Node, right: Node) -> Node {
    with(|manager| manager.or(left, right))
}

pub fn not(node: Node) -> Node {
    with(|manager| manager.not(node))
}

/// The name of a variable index.
pub fn name(variable: u32) -> String {
    with(|manager| manager.names[variable as usize].clone())
}

/// The index of a name's variable, making one for a new name.
pub fn index(name: &str) -> u32 {
    with(|manager| {
        let node = manager.variable(name);
        manager.nodes[node as usize].0
    })
}

/// The variables a node depends on.
pub fn support(node: Node) -> HashSet<u32> {
    with(|manager| {
        let mut variables = HashSet::new();
        let mut seen = HashSet::new();
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            if node <= TRUE || !seen.insert(node) {
                continue;
            }
            let (variable, low, high) = manager.nodes[node as usize];
            variables.insert(variable);
            stack.push(low);
            stack.push(high);
        }
        variables
    })
}

/// Whether a node holds with each variable as `value` gives it.
pub fn eval(node: Node, value: &dyn Fn(u32) -> bool) -> bool {
    with(|manager| {
        let mut node = node;
        while node > TRUE {
            let (variable, low, high) = manager.nodes[node as usize];
            node = if value(variable) { high } else { low };
        }
        node == TRUE
    })
}

/// An irredundant sum of products covering `lower` within `upper`: each product a list of (variable, positive).
pub fn sum_of_products(lower: Node, upper: Node) -> Vec<Vec<(u32, bool)>> {
    with(|manager| manager.isop(lower, upper, &mut HashMap::new()).0)
}

/// A condition's words, in order of appearance (`true` and `false` left out); None when it does not parse.
pub fn words(text: &str) -> Option<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    for token in tokenize(text)? {
        if let Token::Word(word) = token {
            if word != "true" && word != "false" && !words.contains(&word) {
                words.push(word);
            }
        }
    }
    Some(words)
}

/// A condition (`a & !(b | c)`, with `and`/`or`/`not`, `&&`/`||` and `,` for `&`, as manifests write them) as a
/// node; None when it does not parse. `true` and `false` are constants.
pub fn parse(text: &str) -> Option<Node> {
    let tokens = tokenize(text)?;
    with(|manager| {
        let mut operands: Vec<Node> = Vec::new();
        let mut operators: Vec<Token> = Vec::new();
        let mut expect_operand = true;

        fn apply(manager: &mut Manager, operands: &mut Vec<Node>, operator: &Token) -> Option<()> {
            let right = operands.pop()?;
            let result = match operator {
                Token::Not => manager.not(right),
                Token::And => {
                    let left = operands.pop()?;
                    manager.and(left, right)
                }
                Token::Or => {
                    let left = operands.pop()?;
                    manager.or(left, right)
                }
                _ => return None,
            };
            operands.push(result);
            Some(())
        }
        let precedence = |token: &Token| match token {
            Token::Not => 3,
            Token::And => 2,
            Token::Or => 1,
            _ => 0,
        };

        for token in tokens {
            match token {
                Token::Word(word) => {
                    if !expect_operand {
                        return None;
                    }
                    operands.push(match word.as_str() {
                        "true" => TRUE,
                        "false" => FALSE,
                        _ => manager.variable(&word),
                    });
                    expect_operand = false;
                    while matches!(operators.last(), Some(Token::Not)) {
                        let operator = operators.pop()?;
                        apply(manager, &mut operands, &operator)?;
                    }
                }
                Token::Not | Token::Open => {
                    if !expect_operand {
                        return None;
                    }
                    operators.push(token);
                }
                Token::And | Token::Or => {
                    if expect_operand {
                        return None;
                    }
                    while let Some(top) = operators.last() {
                        if *top == Token::Open || precedence(top) < precedence(&token) {
                            break;
                        }
                        let operator = operators.pop()?;
                        apply(manager, &mut operands, &operator)?;
                    }
                    operators.push(token);
                    expect_operand = true;
                }
                Token::Close => {
                    if expect_operand {
                        return None;
                    }
                    loop {
                        let operator = operators.pop()?;
                        if operator == Token::Open {
                            break;
                        }
                        apply(manager, &mut operands, &operator)?;
                    }
                    while matches!(operators.last(), Some(Token::Not)) {
                        let operator = operators.pop()?;
                        apply(manager, &mut operands, &operator)?;
                    }
                }
            }
        }

        if expect_operand {
            return None;
        }
        while let Some(operator) = operators.pop() {
            if operator == Token::Open {
                return None;
            }
            apply(manager, &mut operands, &operator)?;
        }
        match operands.as_slice() {
            [node] => Some(*node),
            _ => None,
        }
    })
}

#[derive(PartialEq)]
enum Token {
    Word(String),
    Not,
    And,
    Or,
    Open,
    Close,
}

fn tokenize(text: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut characters = text.chars().peekable();

    while let Some(&character) = characters.peek() {
        if character.is_whitespace() {
            characters.next();
            continue;
        }
        if "!&|(),".contains(character) {
            characters.next();
            if (character == '&' || character == '|') && characters.peek() == Some(&character) {
                characters.next();
            }
            tokens.push(match character {
                '!' => Token::Not,
                '&' | ',' => Token::And,
                '|' => Token::Or,
                '(' => Token::Open,
                _ => Token::Close,
            });
            continue;
        }
        if !(character.is_ascii_alphanumeric() || "_-.+".contains(character)) {
            return None;
        }

        let mut word = String::new();
        while let Some(&character) = characters.peek() {
            if !(character.is_ascii_alphanumeric() || "_-.+".contains(character)) {
                break;
            }
            word.push(character);
            characters.next();
        }
        tokens.push(match word.as_str() {
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            _ => Token::Word(word),
        });
    }

    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_as_conditions_do() {
        let a = variable("bdd_a");
        let b = variable("bdd_b");
        let c = variable("bdd_c");
        assert_eq!(parse("bdd_a & !bdd_b | bdd_c"), Some(or(and(a, not(b)), c)));
        assert_eq!(parse("!(bdd_a | bdd_b) && bdd_c"), Some(and(not(or(a, b)), c)));
        assert_eq!(parse("not bdd_a or bdd_a"), Some(TRUE));
        assert_eq!(parse("bdd_a, bdd_b"), Some(and(a, b)));
        assert_eq!(parse("!!bdd_a"), Some(a));
        for broken in ["", "bdd_a &", "(bdd_a", "bdd_a)", "bdd_a bdd_b", "& bdd_a", "bdd_a $"] {
            assert_eq!(parse(broken), None, "{broken}");
        }
    }

    #[test]
    fn deep_conditions_do_not_recurse() {
        let deep = format!("{}bdd_a{}", "(".repeat(100_000), ")".repeat(100_000));
        assert_eq!(parse(&deep), parse("bdd_a"));
        let long = (0..1000).map(|index| format!("!bdd_n{index}")).collect::<Vec<_>>().join(" & ");
        assert!(parse(&long).is_some());
    }

    #[test]
    fn sums_of_products() {
        let condition = parse("(bdd_x & bdd_y) | (bdd_x & !bdd_y)").unwrap();
        let cubes = sum_of_products(condition, condition);
        assert_eq!(cubes, vec![vec![(index("bdd_x"), true)]]);
    }
}
