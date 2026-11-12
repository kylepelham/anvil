//! Tests of one kind (made by one CMake helper, or running one program and script) as one `[[test]]` with a matrix
//! (crate::test_cases). What the tests share stays in the template; words that differ become parameters, named after
//! the foreach variables, helper arguments or options they came from; other settings that differ go to the entries.
//! The tests are then factored into axes, so CMake's nested foreach loops come back as one axis each.

use super::emit::TestOut;
use super::model::HelperCall;
use crate::test_cases::TEST_KEYS;
use indexmap::IndexMap;

// Fewer tests than this of one kind stay as they are
const MIN_CASES: usize = 3;

// Characters a shared prefix may end on and a shared suffix start with, so parameters are whole words or path parts
const PREFIX_ENDS: &[char] = &['/', '=', ':', ',', ';', ' '];
const SUFFIX_STARTS: &[char] = &['/', '=', ':', ',', ';', ' ', '.'];

// Files a command runs as a script: tests running different ones are not one kind of test
const SCRIPT_EXTENSIONS: &[&str] = &[".py", ".cmake", ".sh", ".ps1", ".bat", ".cmd", ".js", ".rb", ".pl", ".lua"];

// Separators CMake helpers join lists with inside one argument
const LIST_SEPARATORS: &[&str] = &["@@", ";"];

// A test name's shared start and end must be this long to make the rest a parameter
const MIN_NAME_AFFIX: usize = 4;

/// A test's setting as the manifest writes it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Field {
    Text(String),
    List(Vec<String>),
    /// Written as is (a number, a boolean).
    Literal(String),
    Env(Vec<(String, String)>),
}

pub(super) struct Group {
    /// The helper the tests come from.
    pub function: Option<String>,
    pub count: usize,
    /// The tests' name with parameters; None when every entry names its tests.
    pub name: Option<String>,
    pub template: Vec<(&'static str, Field)>,
    pub axes: Vec<Axis>,
}

pub(super) struct Axis {
    pub name: String,
    /// Each entry's parameters and settings. An entry setting only the parameter named after the axis is written as
    /// its value.
    pub entries: Vec<Vec<(String, Field)>>,
}

pub(super) enum Entry<'a> {
    Alone(&'a TestOut),
    /// A group, and the tests it stands for (to write instead when it is not shorter).
    Group(Group, Vec<&'a TestOut>),
}

/// The tests to write, in order: groups where enough tests are of one kind, the rest alone. `fields` gives a test's
/// settings but its name; `convert_word` turns a CMake argument into the manifest's spelling.
pub(super) fn group_tests<'a>(
    tests: &'a [TestOut],
    fields: &dyn Fn(&TestOut) -> Vec<(&'static str, Field)>,
    convert_word: &dyn Fn(&str) -> String,
) -> Vec<Entry<'a>> {
    // Tests of one kind; tests with notes stay alone, their TODOs beside them
    let mut kinds: IndexMap<String, Vec<&TestOut>> = IndexMap::new();
    for (index, test) in tests.iter().enumerate() {
        let kind = match &test.helper {
            _ if !test.properties.notes.is_empty() => format!("alone {index}"),
            Some(helper) => helper.function.clone(),
            None => command_signature(&test.command),
        };
        kinds.entry(kind).or_default().push(test);
    }

    let mut entries = Vec::new();
    for members in kinds.into_values() {
        for members in split_by_options(members) {
            if members.len() < MIN_CASES {
                entries.extend(members.into_iter().map(Entry::Alone));
                continue;
            }

            let group = build_group(&members, fields, convert_word);
            entries.push(Entry::Group(group, members));
        }
    }

    entries
}

/// Tests added directly as groups whose commands line up word by word: all of them when their options differ in one
/// short run only (`... -- --no-hwrt`), else those with the same options, and the rest together.
fn split_by_options(members: Vec<&TestOut>) -> Vec<Vec<&TestOut>> {
    let skeletons: Vec<Vec<String>> = members.iter().map(|test| command_options(&test.command)).collect();
    let (start, end) = shared_ends(&skeletons);
    let longest_run = skeletons.iter().map(|skeleton| skeleton.len() - start - end).max().unwrap_or(0);
    if members[0].helper.is_some() || start + end >= 2 * longest_run {
        return vec![members];
    }

    let mut classes: IndexMap<String, Vec<&TestOut>> = IndexMap::new();
    for test in members {
        classes.entry(command_options(&test.command).join(" ")).or_default().push(test);
    }

    let mut groups = Vec::new();
    let mut rest = Vec::new();
    for class in classes.into_values() {
        if class.len() >= MIN_CASES {
            groups.push(class);
        } else {
            rest.extend(class);
        }
    }
    if !rest.is_empty() {
        groups.push(rest);
    }
    groups
}

fn build_group(members: &[&TestOut], fields: &dyn Fn(&TestOut) -> Vec<(&'static str, Field)>, convert_word: &dyn Fn(&str) -> String) -> Group {
    let all_fields: Vec<Vec<(&'static str, Field)>> = members.iter().map(|test| fields(test)).collect();
    let mut keys: Vec<&'static str> = Vec::new();
    for (key, _) in all_fields.iter().flatten() {
        if !keys.contains(key) {
            keys.push(key);
        }
    }

    // Each setting: the same everywhere, a template with parameters (the command, its folder and environment), or
    // each test's own
    let mut builder = Builder {
        members,
        convert_word,
        parameters: Vec::new(),
    };
    let mut template = Vec::new();
    let mut columns: Vec<Column> = Vec::new();
    for key in keys {
        let values: Vec<Option<Field>> = all_fields
            .iter()
            .map(|fields| fields.iter().find(|(name, _)| *name == key).map(|(_, field)| field.clone()))
            .collect();
        if values.iter().all(|value| *value == values[0]) {
            template.extend(values[0].clone().map(|field| (key, field)));
            continue;
        }

        let present: Option<Vec<Field>> = values.iter().cloned().collect();
        match present.and_then(|fields| builder.unify_field(key, &fields)) {
            Some(field) => template.push((key, field)),
            None => columns.push(Column {
                key: key.to_string(),
                values,
            }),
        }
    }

    // The name: a pattern over parameters, else each test's own
    let names: Vec<String> = members.iter().map(|test| test.name.clone()).collect();
    let name = builder.name_pattern(&names);
    if name.is_none() {
        columns.push(Column {
            key: "name".to_string(),
            values: names.into_iter().map(|name| Some(Field::Text(name))).collect(),
        });
    }

    let name_parameters: Vec<String> = builder.parameters.iter().map(|parameter| parameter.name.clone()).collect();
    let mut all_columns: Vec<Column> = builder
        .parameters
        .into_iter()
        .map(|parameter| Column {
            key: parameter.name,
            values: parameter.values.into_iter().map(Some).collect(),
        })
        .collect();
    all_columns.extend(columns);

    Group {
        function: members[0].helper.as_ref().map(|helper| helper.function.clone()),
        count: members.len(),
        axes: factor(all_columns, name.as_deref(), &name_parameters),
        name,
        template,
    }
}

/// A parameter or setting: its value in each test (None: the test has no such setting).
struct Column {
    key: String,
    values: Vec<Option<Field>>,
}

/// The tests as a product of axes, each a set of columns that vary together; one axis when they are not a product.
fn factor(columns: Vec<Column>, name: Option<&str>, parameters: &[String]) -> Vec<Axis> {
    let count = columns.first().map(|column| column.values.len()).unwrap_or(0);
    let value_ids: Vec<Vec<usize>> = columns.iter().map(|column| ids(&column.values)).collect();

    // Columns go together when one decides the other
    let mut component: Vec<usize> = (0..columns.len()).collect();
    for left in 0..columns.len() {
        for right in 0..columns.len() {
            if left != right && decides(&value_ids[left], &value_ids[right]) {
                let (from, to) = (component[right], component[left]);
                component.iter_mut().filter(|id| **id == from).for_each(|id| *id = to);
            }
        }
    }
    let mut groups: IndexMap<usize, Vec<usize>> = IndexMap::new();
    for (index, id) in component.iter().enumerate() {
        groups.entry(*id).or_default().push(index);
    }
    let mut groups: Vec<Vec<usize>> = groups.into_values().collect();

    // A product: as many tests as combinations, each once
    let tuple_ids = |group: &[usize]| {
        ids(&(0..count)
            .map(|test| group.iter().map(|column| value_ids[*column][test]).collect::<Vec<_>>())
            .collect::<Vec<_>>())
    };
    let per_group: Vec<Vec<usize>> = groups.iter().map(|group| tuple_ids(group)).collect();
    let combinations: usize = per_group.iter().map(|ids| ids.iter().max().map_or(1, |max| max + 1)).product();
    let distinct = ids(&(0..count)
        .map(|test| per_group.iter().map(|ids| ids[test]).collect::<Vec<_>>())
        .collect::<Vec<_>>());
    let is_product = combinations == count && distinct.iter().max().map_or(0, |max| max + 1) == count;
    if !is_product {
        groups = vec![(0..columns.len()).collect()];
    } else {
        // Outermost first: the axis whose value changes least often in CMake's order
        let changes = |group: &Vec<usize>| {
            let ids = tuple_ids(group);
            ids.windows(2).filter(|pair| pair[0] != pair[1]).count()
        };
        groups.sort_by_key(changes);
    }

    let mut axes: Vec<Axis> = Vec::new();
    for group in groups {
        let tuples = tuple_ids(&group);
        let first_tests: Vec<usize> = (0..=tuples.iter().copied().max().unwrap_or(0))
            .map(|id| tuples.iter().position(|tuple| *tuple == id).unwrap())
            .collect();

        // The axis is named after a text parameter that differs in every entry, one in the name first
        let differs = |column: usize| {
            ids(&first_tests.iter().map(|test| value_ids[column][*test]).collect::<Vec<_>>())
                .iter()
                .max()
                == Some(&(first_tests.len() - 1))
        };
        let is_text = |column: usize| {
            first_tests
                .iter()
                .all(|test| matches!(columns[column].values[*test], Some(Field::Text(_))))
        };
        let in_name = |column: usize| name.is_some_and(|name| name.contains(&format!("{{{}}}", columns[column].key)));
        let usable = |column: &usize| {
            let key = &columns[*column].key;
            (parameters.contains(key) || key == "name") && is_text(*column) && differs(*column)
        };
        let label = group
            .iter()
            .copied()
            .filter(usable)
            .min_by_key(|column| (!in_name(*column), columns[*column].key.len()));
        let axis_name = match label {
            Some(column) => columns[column].key.clone(),
            None => (1..)
                .map(|number| if number == 1 { "case".to_string() } else { format!("case-{number}") })
                .find(|candidate| !axes.iter().any(|axis| axis.name == *candidate) && !columns.iter().any(|column| column.key == *candidate))
                .unwrap(),
        };

        // The axis's own parameter first
        let mut order = group.clone();
        order.sort_by_key(|column| Some(*column) != label);
        let entries = first_tests
            .iter()
            .map(|test| {
                order
                    .iter()
                    .filter_map(|column| Some((columns[*column].key.clone(), columns[*column].values[*test].clone()?)))
                    .collect()
            })
            .collect();
        axes.push(Axis { name: axis_name, entries });
    }

    axes
}

/// Each value as the index of its first occurrence among the distinct values.
fn ids<T: PartialEq>(values: &[T]) -> Vec<usize> {
    let mut distinct: Vec<&T> = Vec::new();
    values
        .iter()
        .map(|value| match distinct.iter().position(|seen| *seen == value) {
            Some(index) => index,
            None => {
                distinct.push(value);
                distinct.len() - 1
            }
        })
        .collect()
}

/// Does knowing the left value tell the right one?
fn decides(left: &[usize], right: &[usize]) -> bool {
    let mut seen: IndexMap<usize, usize> = IndexMap::new();
    left.iter().zip(right).all(|(left, right)| *seen.entry(*left).or_insert(*right) == *right)
}

struct Parameter {
    name: String,
    values: Vec<Field>,
}

struct Builder<'a> {
    members: &'a [&'a TestOut],
    convert_word: &'a dyn Fn(&str) -> String,
    parameters: Vec<Parameter>,
}

impl<'a> Builder<'a> {
    /// A setting that differs as a template, for those whose parts are worth naming: the command, the output it
    /// expects, its folder and its environment (when it sets the same variables).
    fn unify_field(&mut self, key: &str, fields: &[Field]) -> Option<Field> {
        match &fields[0] {
            Field::List(_) if key == "command" => {
                let lists: Vec<Vec<String>> = fields.iter().map(|field| list_of(field)).collect();
                Some(Field::List(self.unify_list(&lists, "args", "arg")))
            }
            // Expected output that differs in a few lines, or in the test's name
            Field::List(_) if key == "expect" || key == "reject" => {
                let lists: Vec<Vec<String>> = fields.iter().map(|field| list_of(field)).collect();
                Some(Field::List(self.unify_list(&lists, key, key)))
            }
            Field::Text(_) if key == "workdir" => {
                let texts: Vec<String> = fields.iter().map(|field| text_of(field)).collect();
                Some(Field::Text(self.unify_text(&texts, Some("dir"))))
            }
            Field::Env(first) => {
                let names = |field: &Field| match field {
                    Field::Env(entries) => entries.iter().map(|(name, _)| name.clone()).collect(),
                    _ => Vec::new(),
                };
                if fields.iter().any(|field| names(field) != names(&fields[0])) {
                    return None;
                }

                let mut unified = Vec::new();
                for (index, (variable, _)) in first.iter().enumerate() {
                    let texts: Vec<String> = fields
                        .iter()
                        .map(|field| match field {
                            Field::Env(entries) => entries[index].1.clone(),
                            _ => String::new(),
                        })
                        .collect();
                    unified.push((variable.clone(), self.unify_text(&texts, Some(variable))));
                }
                Some(Field::Env(unified))
            }
            _ => None,
        }
    }

    /// The tests' names as one pattern: the foreach variables and helper arguments they were made from
    /// (`still_frames_{path}_{view}`), else their shared start and end around a parameter.
    fn name_pattern(&mut self, names: &[String]) -> Option<String> {
        // Variables known to every test whose values differ between them, longest values first
        let known: Vec<Vec<(String, String)>> = self.members.iter().map(|test| known_variables(test)).collect();
        let mut variables: Vec<String> = Vec::new();
        for (variable, _) in &known[0] {
            let values: Vec<Option<&String>> = known
                .iter()
                .map(|known| known.iter().rev().find(|(name, _)| name == variable).map(|(_, value)| value))
                .collect();
            let varies = values.iter().any(|value| *value != values[0]);
            if varies && values.iter().all(|value| value.is_some_and(|value| !value.is_empty())) && !variables.contains(variable) {
                variables.push(variable.clone());
            }
        }
        let value_of = |test: usize, variable: &str| {
            known[test]
                .iter()
                .rev()
                .find(|(name, _)| name == variable)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        variables.sort_by_key(|variable| std::cmp::Reverse(value_of(0, variable).len()));

        // Each name with those values marked; one pattern for all of them
        let patterns: Vec<(String, Vec<usize>)> = names
            .iter()
            .enumerate()
            .map(|(test, name)| {
                let mut pattern = name.clone();
                let mut used = Vec::new();
                for (index, variable) in variables.iter().enumerate() {
                    if let Some(marked) = mark_word(&pattern, &value_of(test, variable), index) {
                        pattern = marked;
                        used.push(index);
                    }
                }
                used.sort();
                (pattern, used)
            })
            .collect();
        // (a whole name is no pattern: the entries name their tests)
        let same = patterns.iter().all(|pattern| *pattern == patterns[0]);
        let whole = patterns[0].0.chars().count() == 1;
        if same && !patterns[0].1.is_empty() && !whole {
            let mut pattern = patterns[0].0.clone();
            for index in &patterns[0].1 {
                let values: Vec<Field> = (0..names.len()).map(|test| Field::Text(value_of(test, &variables[*index]))).collect();
                let parameter = self.named_parameter(values, &variables[*index]);
                pattern = pattern.replace(marker(*index), &format!("{{{parameter}}}"));
            }
            return Some(pattern);
        }

        // Else the shared start and end around one parameter
        let prefix = common_prefix(names);
        let prefix = &prefix[..prefix.rfind(|character: char| !character.is_ascii_alphanumeric()).map_or(0, |at| at + 1)];
        let suffix = common_suffix(names, prefix.len());
        let suffix = &suffix[suffix.find(|character: char| !character.is_ascii_alphanumeric()).unwrap_or(suffix.len())..];
        let middles: Vec<Field> = names
            .iter()
            .map(|name| Field::Text(name[prefix.len()..name.len() - suffix.len()].to_string()))
            .collect();
        if middles.iter().any(|middle| *middle == Field::Text(String::new())) {
            return None;
        }
        let existing = self
            .parameters
            .iter()
            .find(|parameter| parameter.values == middles)
            .map(|parameter| parameter.name.clone());
        let parameter = match existing {
            Some(existing) if !prefix.is_empty() || !suffix.is_empty() => existing,
            None if prefix.len() + suffix.len() >= MIN_NAME_AFFIX => self.named_parameter(middles, "case"),
            _ => return None,
        };
        Some(format!("{prefix}{{{parameter}}}{suffix}"))
    }

    /// The parameter with these values: an earlier one, else a new one called `name`.
    fn named_parameter(&mut self, values: Vec<Field>, name: &str) -> String {
        if let Some(existing) = self.parameters.iter().find(|parameter| parameter.values == values) {
            return existing.name.clone();
        }

        let name = self.free_name(&[parameter_spelling(name)]);
        self.parameters.push(Parameter { name: name.clone(), values });
        name
    }

    /// One text per case as a template: their shared start and end around a parameter, named after the option it
    /// is the value of (`-DSCRATCH=`, or `hint`: the option before it) when nothing better names it.
    fn unify_text(&mut self, texts: &[String], hint: Option<&str>) -> String {
        if texts.iter().all(|text| *text == texts[0]) {
            return texts[0].clone();
        }

        // A list joined into the text: the parameter is the whole list, so the shared part stops before it
        let separator = LIST_SEPARATORS
            .iter()
            .copied()
            .find(|separator| texts.iter().any(|text| text.contains(separator)));
        let mut prefix = common_prefix(texts);
        let mut suffix = common_suffix(texts, prefix.len());
        if let Some(separator) = separator {
            if let Some(at) = prefix.find(separator) {
                prefix.truncate(at);
            }
            if let Some(at) = suffix.rfind(separator) {
                suffix = suffix[at + separator.len()..].to_string();
            }
        }
        let prefix = snap_prefix(&prefix);
        let suffix = snap_suffix(&suffix, texts, prefix.len());

        let middles: Vec<String> = texts
            .iter()
            .map(|text| text[prefix.len()..text.len() - suffix.len()].to_string())
            .collect();
        let extension = extension_name(&suffix);
        let hint = option_name(&prefix).filter(|_| prefix.contains('=')).or(extension).or(hint);
        let placeholder = self.text_parameter(&middles, separator, hint);
        format!("{prefix}{placeholder}{suffix}")
    }

    /// One list per case: item by item when they are as long, else the shared start and end around one list
    /// parameter.
    fn unify_list(&mut self, lists: &[Vec<String>], hint: &str, item_name: &str) -> Vec<String> {
        let length = lists[0].len();
        if lists.iter().all(|list| list.len() == length) {
            return (0..length)
                .map(|index| {
                    let texts: Vec<String> = lists.iter().map(|list| list[index].clone()).collect();
                    let item_hint = list_hint(&lists[0][..index]).unwrap_or_else(|| item_name.to_string());
                    self.unify_text(&texts, Some(&item_hint))
                })
                .collect();
        }

        // Else the words that line up at the start and end (same options) item by item, around one list
        let skeletons: Vec<Vec<String>> = lists.iter().map(|list| command_options(list)).collect();
        let (start, end) = shared_ends(&skeletons);
        let mut result: Vec<String> = (0..start)
            .map(|index| {
                let texts: Vec<String> = lists.iter().map(|list| list[index].clone()).collect();
                let item_hint = list_hint(&lists[0][..index]).unwrap_or_else(|| item_name.to_string());
                self.unify_text(&texts, Some(&item_hint))
            })
            .collect();

        let middles: Vec<Field> = lists.iter().map(|list| Field::List(list[start..list.len() - end].to_vec())).collect();
        result.push(format!("{{{}}}", self.parameter(middles, Some(hint))));
        for back in (1..=end).rev() {
            let texts: Vec<String> = lists.iter().map(|list| list[list.len() - back].clone()).collect();
            result.push(self.unify_text(&texts, Some(item_name)));
        }
        result
    }

    /// The placeholder for a part of a text that differs: `{name}` when it is the test's name, else a parameter
    /// (a list one when the parts are lists joined with `separator`).
    fn text_parameter(&mut self, middles: &[String], separator: Option<&str>, hint: Option<&str>) -> String {
        let is_name = middles.iter().zip(self.members).all(|(middle, test)| *middle == test.name);
        if is_name {
            return "{name}".to_string();
        }

        match separator {
            Some(separator) => {
                let values = middles
                    .iter()
                    .map(|middle| Field::List(middle.split(separator).filter(|item| !item.is_empty()).map(str::to_string).collect()))
                    .collect();
                format!("{{{}:{separator}}}", self.parameter(values, hint))
            }
            None => {
                let values = middles.iter().map(|middle| Field::Text(middle.clone())).collect();
                format!("{{{}}}", self.parameter(values, hint))
            }
        }
    }

    /// The parameter holding these values: an earlier one with the same values, else a new one.
    fn parameter(&mut self, values: Vec<Field>, hint: Option<&str>) -> String {
        if let Some(existing) = self.parameters.iter().find(|parameter| parameter.values == values) {
            return existing.name.clone();
        }

        let name = self.parameter_name(&values, hint);
        self.parameters.push(Parameter { name: name.clone(), values });
        name
    }

    /// The foreach variable or helper argument these values came from, the same in most cases; else the option's
    /// name (`hint`); else `value`.
    fn parameter_name(&self, values: &[Field], hint: Option<&str>) -> String {
        let mut votes: IndexMap<String, usize> = IndexMap::new();
        for (value, test) in values.iter().zip(self.members) {
            if let Some(name) = self.variable_name(test, value) {
                *votes.entry(name).or_default() += 1;
            }
        }

        let mut candidates: Vec<(String, usize)> = votes.into_iter().collect();
        candidates.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        candidates.extend(hint.map(|hint| (parameter_spelling(hint), 0)));
        let candidates: Vec<String> = candidates
            .into_iter()
            .map(|(candidate, _)| if candidate == "command" { "program".to_string() } else { candidate })
            .collect();
        self.free_name(&candidates)
    }

    /// The first candidate no parameter or test key has, else the first numbered.
    fn free_name(&self, candidates: &[String]) -> String {
        let taken = |name: &str| TEST_KEYS.contains(&name) || self.parameters.iter().any(|parameter| parameter.name == name);
        if let Some(free) = candidates.iter().find(|candidate| !taken(candidate)) {
            return free.clone();
        }

        let base = candidates.first().cloned().unwrap_or_else(|| "value".to_string());
        let numbered = (2..).map(|number| format!("{base}-{number}"));
        std::iter::once(base.clone()).chain(numbered).find(|name| !taken(name)).unwrap()
    }

    /// What CMake called the value in this test: a foreach variable holding it, the helper's parameter at its
    /// position, or the keyword before it in the call (`SUBDIRS a b` -> subdirs).
    fn variable_name(&self, test: &TestOut, value: &Field) -> Option<String> {
        let first = match value {
            Field::Text(text) => text.clone(),
            Field::List(items) => items.first()?.clone(),
            _ => return None,
        };
        let matches = |argument: &String| {
            let converted = (self.convert_word)(argument);
            converted == first || format!("{{target:{argument}}}") == first || converted.split("@@").next() == Some(first.as_str())
        };

        if let Field::Text(text) = value {
            if let Some((variable, _)) = test.loops.iter().rev().find(|(_, item)| (self.convert_word)(item) == *text) {
                return Some(parameter_spelling(variable));
            }
        }

        let helper: &HelperCall = test.helper.as_ref()?;
        let position = helper.args.iter().position(matches)?;
        if position < helper.params.len() {
            return Some(parameter_spelling(&helper.params[position]));
        }

        helper.args[..position]
            .iter()
            .rev()
            .find(|argument| is_keyword(argument))
            .map(|keyword| parameter_spelling(keyword))
    }
}

/// The variables a test was made with: its foreach loops', then its helper's parameters.
fn known_variables(test: &TestOut) -> Vec<(String, String)> {
    let mut known = test.loops.clone();
    if let Some(helper) = &test.helper {
        known.extend(helper.params.iter().cloned().zip(helper.args.iter().cloned()));
    }
    known
}

/// A stand-in for a variable in a name being made a pattern (a character no name has).
fn marker(index: usize) -> char {
    char::from_u32(0xE000 + index as u32).unwrap()
}

/// The text with `value` marked where it is a whole word (between non-alphanumerics or the ends), first such place.
fn mark_word(text: &str, value: &str, index: usize) -> Option<String> {
    let is_boundary = |character: Option<char>| character.is_none_or(|character| !character.is_ascii_alphanumeric());
    let (at, _) = text
        .match_indices(value)
        .find(|(at, _)| is_boundary(text[..*at].chars().next_back()) && is_boundary(text[at + value.len()..].chars().next()))?;
    Some(format!("{}{}{}", &text[..at], marker(index), &text[at + value.len()..]))
}

/// What a command runs: its program, and the script it hands that program, if any (`python tools/x.py`,
/// `cmake -P tests/y.cmake`). Through `cmake -E env A=b`, the program it starts; `cmake -E <tool>` otherwise.
fn command_signature(command: &[String]) -> String {
    let mut command = command;
    if command.len() > 2 && command[0] == "cmake" && command[1] == "-E" {
        if command[2] != "env" {
            return format!("cmake -E {}", command[2]);
        }
        let start = 3 + command[3..].iter().take_while(|word| word.contains('=') || word.starts_with('-')).count();
        command = &command[start.min(command.len())..];
    }

    let program = command.first().cloned().unwrap_or_default();
    let script = command[1.min(command.len())..].iter().find(|word| {
        let lower = word.to_lowercase();
        SCRIPT_EXTENSIONS.iter().any(|extension| lower.ends_with(extension))
    });

    match script {
        Some(script) => format!("{program} {script}"),
        None => program,
    }
}

/// A command's options in order, its other words as `*`: commands alike in this line up word by word.
fn command_options(command: &[String]) -> Vec<String> {
    command
        .iter()
        .map(|word| match option_name(word) {
            Some(_) if word.starts_with("-D") => word.split_inclusive('=').next().unwrap_or(word).to_string(),
            Some(_) => word.clone(),
            None => "*".to_string(),
        })
        .collect()
}

/// How many words every list shares at its start, and then at its end.
fn shared_ends(lists: &[Vec<String>]) -> (usize, usize) {
    let shortest = lists.iter().map(Vec::len).min().unwrap_or(0);
    let start = (0..shortest)
        .take_while(|index| lists.iter().all(|list| list[*index] == lists[0][*index]))
        .count();
    let end = (0..shortest - start)
        .take_while(|index| {
            lists
                .iter()
                .all(|list| list[list.len() - 1 - index] == lists[0][lists[0].len() - 1 - index])
        })
        .count();
    (start, end)
}

/// The name of an option word: `-DSCRATCH=x` -> SCRATCH, `--out` -> out, `--out=x` -> out.
fn option_name(word: &str) -> Option<&str> {
    let name = if let Some(define) = word.strip_prefix("-D") {
        define.split_once('=')?.0
    } else {
        let flag = word.strip_prefix("--").or_else(|| word.strip_prefix('-'))?;
        flag.split('=').next().unwrap_or(flag)
    };

    let is_name = name.chars().next().is_some_and(|character| character.is_ascii_alphabetic())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-');
    is_name.then_some(name)
}

/// A name for the next item of a list from the items before it: the option just before (`--out x`), else the
/// nearest `-DKEY=` item (its value went on as more items).
fn list_hint(before: &[String]) -> Option<String> {
    let last = before.last()?;
    if !last.contains('=') && last.starts_with('-') {
        return option_name(last).map(str::to_string);
    }

    before
        .iter()
        .rev()
        .find(|item| item.starts_with("-D") && item.contains('='))
        .and_then(|item| option_name(item))
        .map(str::to_string)
}

/// The extension a shared end starts with: `.png` -> png.
fn extension_name(suffix: &str) -> Option<&str> {
    let extension = suffix.strip_prefix('.')?;
    let end = extension
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(extension.len());
    let extension = &extension[..end];
    extension
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        .then_some(extension)
}

/// `SUBDIRS` -> `subdirs`, `test_dir` -> `test-dir`.
fn parameter_spelling(text: &str) -> String {
    text.to_lowercase().replace('_', "-")
}

/// An all-capitals word, as cmake_parse_arguments keywords are.
fn is_keyword(text: &str) -> bool {
    text.len() > 1
        && text
            .chars()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_')
        && text.chars().next().is_some_and(|character| character.is_ascii_uppercase())
}

fn text_of(field: &Field) -> String {
    match field {
        Field::Text(text) | Field::Literal(text) => text.clone(),
        Field::List(items) => items.join(" "),
        Field::Env(_) => String::new(),
    }
}

fn list_of(field: &Field) -> Vec<String> {
    match field {
        Field::List(items) => items.clone(),
        other => vec![text_of(other)],
    }
}

fn common_prefix(texts: &[String]) -> String {
    let mut prefix: &str = &texts[0];
    for text in &texts[1..] {
        let length = prefix
            .char_indices()
            .zip(text.chars())
            .take_while(|((_, left), right)| left == right)
            .last()
            .map(|((index, character), _)| index + character.len_utf8())
            .unwrap_or(0);
        prefix = &prefix[..length];
    }
    prefix.to_string()
}

/// The end every text shares, not reaching into the first `skip` bytes.
fn common_suffix(texts: &[String], skip: usize) -> String {
    let mut suffix: &str = &texts[0][skip..];
    for text in &texts[1..] {
        let rest = &text[skip.min(text.len())..];
        let length = suffix
            .char_indices()
            .rev()
            .zip(rest.chars().rev())
            .take_while(|((_, left), right)| left == right)
            .last()
            .map(|((index, _), _)| suffix.len() - index)
            .unwrap_or(0);
        suffix = &suffix[suffix.len() - length..];
    }
    suffix.to_string()
}

/// The shared start cut back to a word boundary, outside any placeholder.
fn snap_prefix(prefix: &str) -> String {
    let mut end = prefix.len();
    while end > 0 {
        let candidate = &prefix[..end];
        let ends_on_boundary = candidate.ends_with(PREFIX_ENDS);
        if ends_on_boundary && candidate.matches('{').count() == candidate.matches('}').count() {
            return candidate.to_string();
        }
        end = candidate.char_indices().last().map(|(index, _)| index).unwrap_or(0);
    }
    String::new()
}

/// The shared end cut forward to a word boundary, outside any placeholder, and not overlapping the prefix.
fn snap_suffix(suffix: &str, texts: &[String], prefix_length: usize) -> String {
    let shortest = texts.iter().map(String::len).min().unwrap_or(0);
    let mut start = 0;
    while start < suffix.len() {
        let candidate = &suffix[start..];
        let fits = candidate.len() + prefix_length <= shortest;
        let balanced = candidate.matches('{').count() == candidate.matches('}').count();
        if fits && balanced && candidate.starts_with(SUFFIX_STARTS) {
            return candidate.to_string();
        }
        start += candidate.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::model::TestProperties;

    fn test(name: &str, command: &[&str], loops: &[(&str, &str)]) -> TestOut {
        TestOut {
            name: name.to_string(),
            command: command.iter().map(|word| word.to_string()).collect(),
            workdir: None,
            condition: None,
            properties: TestProperties::default(),
            helper: None,
            dir: std::path::PathBuf::new(),
            loops: loops.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
        }
    }

    fn fields(test: &TestOut) -> Vec<(&'static str, Field)> {
        let mut fields = vec![("command", Field::List(test.command.clone()))];
        if !test.properties.after.is_empty() {
            fields.push(("after", Field::List(test.properties.after.clone())));
        }
        fields
    }

    fn one_group(tests: &[TestOut]) -> Group {
        let mut entries = group_tests(tests, &fields, &|word| word.to_string());
        match entries.pop() {
            Some(Entry::Group(group, _)) if entries.is_empty() => group,
            _ => panic!("not one group"),
        }
    }

    #[test]
    fn nested_loops_become_axes() {
        // foreach(path rt cpu) foreach(view street arch glass): the path decides its flag; glass needs assets
        let mut tests = Vec::new();
        for (path, flag) in [("rt", "--rt"), ("cpu", "--cpu")] {
            for view in ["street", "arch", "glass"] {
                let name = format!("stills_{path}_{view}");
                let mut test = test(
                    &name,
                    &["py", "stills.py", "--out", &format!("{{out}}/{name}"), "--view", view, flag],
                    &[("path", path), ("view", view)],
                );
                if view == "glass" {
                    test.properties.after = vec!["assets".into()];
                }
                tests.push(test);
            }
        }

        let group = one_group(&tests);
        assert_eq!(group.name.as_deref(), Some("stills_{path}_{view}"));
        assert_eq!(
            group.template[0].1,
            Field::List(
                ["py", "stills.py", "--out", "{out}/{name}", "--view", "{view}", "{arg}"]
                    .map(String::from)
                    .to_vec()
            )
        );

        let axes: Vec<&str> = group.axes.iter().map(|axis| axis.name.as_str()).collect();
        assert_eq!(axes, ["path", "view"]);
        assert_eq!(
            group.axes[0].entries[1],
            [
                ("path".to_string(), Field::Text("cpu".into())),
                ("arg".to_string(), Field::Text("--cpu".into()))
            ]
        );
        assert_eq!(group.axes[1].entries[0], [("view".to_string(), Field::Text("street".into()))]);
        assert_eq!(
            group.axes[1].entries[2],
            [
                ("view".to_string(), Field::Text("glass".into())),
                ("after".to_string(), Field::List(vec!["assets".into()]))
            ]
        );
    }

    #[test]
    fn names_without_loops_share_their_ends() {
        let tests: Vec<TestOut> = ["ao", "fog", "sky"]
            .iter()
            .map(|pass| test(&format!("{pass}_layout"), &["layout_tests", "{out}/shaders", pass], &[]))
            .collect();

        let group = one_group(&tests);
        assert_eq!(group.name.as_deref(), Some("{arg}_layout"));
        assert_eq!(group.axes.len(), 1);
        assert_eq!(group.axes[0].name, "arg");
    }

    #[test]
    fn helper_tests_name_parameters_after_the_helper() {
        let helper = |args: &[&str]| HelperCall {
            function: "scratch_test".into(),
            params: vec!["test".into(), "dir".into()],
            args: args.iter().map(|word| word.to_string()).collect(),
        };
        let mut tests = Vec::new();
        for (name, program) in [
            ("look", "{target:editor}@@--headless"),
            ("walk", "{target:runtime}@@--walk"),
            ("cook", "python@@cook.py"),
        ] {
            let mut test = test(
                name,
                &[
                    "cmake",
                    &format!("-DSCRATCH={{out}}/s/{name}"),
                    &format!("-DCOMMAND={program}"),
                    "-P",
                    "f.cmake",
                ],
                &[],
            );
            let words: Vec<&str> = program.split("@@").collect();
            let mut args = vec![name, "dir", "COMMAND"];
            args.extend(words.iter().map(|word| word.trim_start_matches("{target:").trim_end_matches('}')));
            test.helper = Some(helper(&args));
            tests.push(test);
        }

        let group = one_group(&tests);
        assert_eq!(
            group.template[0].1,
            Field::List(
                ["cmake", "-DSCRATCH={out}/s/{name}", "-DCOMMAND={program:@@}", "-P", "f.cmake"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(group.name, None);
    }

    #[test]
    fn direct_tests_line_up_by_options() {
        let tests = vec![
            test("street", &["python", "stills.py", "--out", "{out}/street", "--view", "s"], &[]),
            test("arch", &["python", "stills.py", "--out", "{out}/arch", "--view", "a"], &[]),
            test("dining", &["python", "stills.py", "--out", "{out}/dining", "--view", "d"], &[]),
            test("hall", &["python", "stills.py", "--out", "{out}/hall", "--view", "hall", "--slow"], &[]),
            test("cellar", &["python", "stills.py", "--out", "{out}/cellar", "--frames", "9"], &[]),
            test("roof", &["python", "stills.py", "--out", "{out}/roof"], &[]),
        ];
        let entries = group_tests(&tests, &fields, &|word| word.to_string());

        // The three with the same options word by word, the rest as one list
        let [Entry::Group(aligned, _), Entry::Group(rest, _)] = entries.as_slice() else {
            panic!("not two groups")
        };
        assert_eq!(
            aligned.template[0].1,
            Field::List(
                ["python", "stills.py", "--out", "{out}/{name}", "--view", "{view}"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(
            rest.template[0].1,
            Field::List(["python", "stills.py", "--out", "{out}/{name}", "{args}"].map(String::from).to_vec())
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(snap_prefix("-DCOMMAND={target:Ona"), "-DCOMMAND=");
        assert_eq!(snap_suffix("r.txt", &["ar.txt".into(), "br.txt".into()], 0), ".txt");
        assert_eq!(option_name("-DGAME_DIR=x"), Some("GAME_DIR"));
        assert_eq!(option_name("{out}/x"), None);
        assert_eq!(extension_name(".onaproject"), Some("onaproject"));
        assert_eq!(list_hint(&["-DEXPECT=a".into(), " b".into()]), Some("EXPECT".into()));
        assert_eq!(mark_word("a_blendroom_b", "room", 0), None);
        assert_eq!(mark_word("x_rt_street", "rt", 0).map(|text| text.contains(marker(0))), Some(true));
    }
}
