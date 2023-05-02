use std::num::ParseIntError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nom::branch::alt;
use nom::bytes::complete::{escaped, is_not, tag, take, take_while1};
use nom::character::complete::{digit1, space1};
use nom::combinator::{all_consuming, map, map_res, opt};
use nom::multi::{fold_many0, separated_list1};
use nom::sequence::{delimited, pair, preceded, separated_pair, terminated, tuple};
use nom::{IResult, Parser};
use serde::Serialize;
use toshi::{
    BoolQuery, ExactTerm, FuzzyQuery, PhraseQuery, Query, RangeQuery, RegexQuery, TermPair,
};

macro_rules! func_dbg {
    ($inp:ident) => {
        #[cfg(test)]
        {
            fn f() {}
            fn type_name_of<T>(_: T) -> &'static str {
                std::any::type_name::<T>()
            }
            let name = type_name_of(f);
            println!("{}({})", &name[..name.len() - 3], $inp);
        }
    };
}

fn identifier(input: &str) -> IResult<&str, &str> {
    take_while1(|ch: char| ch.is_ascii_lowercase() || ch == '_')(input)
}

fn basic_term(input: &str) -> IResult<&str, &str> {
    let parsed = nom::combinator::not(tag("NOT"))
        .and(take_while1(|ch: char| ch.is_alphanumeric() || ch == '_'))
        .parse(input)?;
    Ok((parsed.0, parsed.1 .1))
}

fn quoted(input: &str) -> IResult<&str, &str> {
    delimited(
        tag("\""),
        take_while1(|ch: char| ch.is_alphanumeric() || ch.is_whitespace()),
        tag("\""),
    )(input)
}

fn regex(input: &str) -> IResult<&str, String> {
    map(
        delimited(
            tag("/"),
            escaped(is_not("\\/"), '\\', take(1usize)),
            tag("/"),
        ),
        |s: &str| s.replace("\\/", "/"),
    )(input)
}

enum Bound<T> {
    Greater { than: T, or_equal: bool },
    Less { than: T, or_equal: bool },
}

impl<T> Bound<T> {
    fn from_symbol(symbol: &str, than: T) -> Option<Self> {
        match symbol {
            ">=" => Some(Self::Greater {
                than,
                or_equal: true,
            }),
            ">" => Some(Self::Greater {
                than,
                or_equal: false,
            }),
            "<=" => Some(Self::Less {
                than,
                or_equal: true,
            }),
            "<" => Some(Self::Less {
                than,
                or_equal: false,
            }),
            _ => None,
        }
    }
}

fn str_range_bound(input: &str) -> IResult<&str, Bound<&str>> {
    map(
        pair(alt((tag(">="), tag(">"), tag("<="), tag("<"))), basic_term),
        |(symbol, text)| Bound::from_symbol(symbol, text).unwrap(),
    )(input)
}

fn num_range_bound(input: &str) -> IResult<&str, Bound<i32>> {
    map_res(
        pair(alt((tag(">="), tag(">"), tag("<="), tag("<"))), digit1),
        |(symbol, digits)| {
            let num = i32::from_str_radix(digits, 10)?;
            Ok::<Bound<i32>, ParseIntError>(Bound::from_symbol(symbol, num).unwrap())
        },
    )(input)
}

fn fuzzy_basic_term(input: &str) -> IResult<&str, (Option<&str>, &str)> {
    preceded(
        tag("~"),
        tuple((opt(terminated(digit1, tag("~"))), basic_term)),
    )(input)
}

fn fuzzy_term_q(input: &str) -> IResult<&str, Query> {
    map_res(
        alt((
            separated_pair(identifier, tag(":"), fuzzy_basic_term),
            map(fuzzy_basic_term, |term| ("message", term)),
        )),
        |(field_name, (distance_opt, term))| {
            let distance = if let Some(dist_str) = distance_opt {
                u8::from_str_radix(dist_str, 10)?
            } else {
                2u8
            };
            Ok::<Query, std::num::ParseIntError>(
                FuzzyQuery::builder()
                    .for_field(field_name)
                    .with_value(term)
                    .with_distance(distance)
                    .with_transposition()
                    .build(),
            )
        },
    )(input)
}

fn exact_term_q(input: &str) -> IResult<&str, Query> {
    map(
        alt((
            separated_pair(identifier, tag(":"), basic_term),
            map(basic_term, |term| ("message", term)),
        )),
        |(field_name, term)| Query::Exact(ExactTerm::with_term(field_name, term)),
    )(input)
}

fn phrase_q(input: &str) -> IResult<&str, Query> {
    map(
        alt((
            separated_pair(identifier, tag(":"), quoted),
            map(quoted, |phrase| ("message", phrase)),
        )),
        |(field_name, phrase_str)| {
            let words: Vec<String> = phrase_str.split_whitespace().map(String::from).collect();
            Query::Phrase(PhraseQuery::with_phrase(
                field_name.to_string(),
                TermPair::new(words, None),
            ))
        },
    )(input)
}

fn regex_q(input: &str) -> IResult<&str, Query> {
    map(
        alt((
            separated_pair(identifier, tag(":"), regex),
            map(regex, |regex_val| ("message", regex_val)),
        )),
        |(field_name, regex_string)| {
            Query::Regex(RegexQuery::from_str(field_name.to_string(), regex_string))
        },
    )(input)
}

fn bounds_to_range_query<T: Serialize>(field_name: &str, bounds: Vec<Bound<T>>) -> Query {
    let mut builder = RangeQuery::builder().for_field(field_name);
    for bound in bounds {
        match bound {
            Bound::Greater {
                than,
                or_equal: true,
            } => builder = builder.gte(Some(than)),
            Bound::Greater {
                than,
                or_equal: false,
            } => builder = builder.gt(Some(than)),
            Bound::Less {
                than,
                or_equal: true,
            } => builder = builder.lte(Some(than)),
            Bound::Less {
                than,
                or_equal: false,
            } => builder = builder.lt(Some(than)),
        }
    }
    builder.build()
}

fn range_q(input: &str) -> IResult<&str, Query> {
    alt((
        map(
            // Try parsing as numeric range
            separated_pair(
                identifier,
                tag(":"),
                separated_list1(tag(","), num_range_bound),
            ),
            |(field_name, bounds)| bounds_to_range_query(field_name, bounds),
        ),
        map(
            // Failing that, try parsing as string range
            separated_pair(
                identifier,
                tag(":"),
                separated_list1(tag(","), str_range_bound),
            ),
            |(field_name, bounds)| bounds_to_range_query(field_name, bounds),
        ),
    ))(input)
}

#[derive(Debug, Clone)]
enum QueryTree {
    Simple(Query),
    Not(Box<QueryTree>),
    And(Vec<QueryTree>),
    Or(Vec<QueryTree>),
}

fn simple(input: &str) -> IResult<&str, QueryTree> {
    func_dbg!(input);
    alt((
        map(
            alt((fuzzy_term_q, exact_term_q, phrase_q, regex_q, range_q)),
            QueryTree::Simple,
        ),
        parens,
        not,
    ))(input)
}

fn parens(input: &str) -> IResult<&str, QueryTree> {
    func_dbg!(input);
    delimited(tag("("), expr, tag(")"))(input)
}

fn not(input: &str) -> IResult<&str, QueryTree> {
    func_dbg!(input);
    map(preceded(pair(tag("NOT"), space1), expr), |q| {
        QueryTree::Not(Box::new(q))
    })(input)
}

fn disjunction(input: &str) -> IResult<&str, QueryTree> {
    func_dbg!(input);
    let (input, init) = simple(input)?;

    fold_many0(
        preceded(tuple((space1, tag("OR"), space1)), simple),
        move || init.clone(),
        |left, right| match left {
            QueryTree::Or(mut xs) => {
                xs.push(right);
                QueryTree::Or(xs)
            }
            _ => QueryTree::Or(vec![left, right]),
        },
    )(input)
}

fn expr(input: &str) -> IResult<&str, QueryTree> {
    func_dbg!(input);
    let (input, init) = disjunction(input)?;

    fold_many0(
        preceded(tuple((space1, tag("AND"), space1)), disjunction),
        move || init.clone(),
        |left, right| match left {
            QueryTree::And(mut xs) => {
                xs.push(right);
                QueryTree::And(xs)
            }
            _ => QueryTree::And(vec![left, right]),
        },
    )(input)
}

fn q_tree_to_query(q_tree: QueryTree, mut reltime: Option<u32>) -> Query {
    // Discard reltime if it's <= 0
    reltime = reltime.filter(|tm| *tm > 0);
    match q_tree {
        QueryTree::Simple(q) => {
            if let Some(time) = reltime {
                BoolQuery::builder()
                    .must_match(q)
                    .must_match(reltime_query(time))
                    .build()
            } else {
                q
            }
        }
        QueryTree::Not(boxed_q) => {
            let mut builder = BoolQuery::builder().must_not_match(q_tree_to_query(*boxed_q, None));
            if let Some(time) = reltime {
                builder = builder.must_match(reltime_query(time))
            }
            builder.build()
        }
        QueryTree::And(sub_qs) => {
            let mut builder = BoolQuery::builder();
            let mut lifted_or_subquery = false;
            for subq in sub_qs {
                match subq {
                    QueryTree::Or(or_sub_qs) if !lifted_or_subquery => {
                        // We can lift all the contents of at-most-one OR clause into should_match in a parent BoolQuery for an AND clause
                        builder = builder.with_minimum_should_match(1);
                        for or_subq in or_sub_qs {
                            builder = builder.should_match(q_tree_to_query(or_subq, None));
                        }
                        lifted_or_subquery = true;
                    }
                    QueryTree::Not(not_subq) => {
                        // We can lift all the NOT clauses into must_not_match in a parent BoolQuery for an AND clause
                        builder = builder.must_not_match(q_tree_to_query(*not_subq, None));
                    }
                    other_q_tree => {
                        // Otherwise just add to must_match
                        builder = builder.must_match(q_tree_to_query(other_q_tree, None));
                    }
                }
            }
            if let Some(time) = reltime {
                builder = builder.must_match(reltime_query(time))
            }
            builder.build()
        }
        QueryTree::Or(sub_qs) => {
            let mut builder = BoolQuery::builder().with_minimum_should_match(1);
            for subq in sub_qs {
                builder = builder.should_match(q_tree_to_query(subq, None));
            }
            if let Some(time) = reltime {
                builder = builder.must_match(reltime_query(time))
            }
            builder.build()
        }
    }
}

pub fn parse_query(input: &str, reltime: Option<u32>) -> IResult<&str, Query> {
    map(all_consuming(expr), |qtree| q_tree_to_query(qtree, reltime))(input)
}

pub fn reltime_query(reltime: u32) -> Query {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards!?");
    let start = now - Duration::from_secs(reltime as u64 * 60);
    RangeQuery::builder()
        .for_field("source_timestamp")
        .gte(Some(start.as_micros()))
        .build()
}

#[cfg(test)]
mod tests {

    use super::expr;
    use super::parse_query;
    use super::range_q;
    use super::regex;
    use super::QueryTree;

    #[test]
    fn test_regex() {
        assert_eq!(
            regex("/simple input/"),
            Ok(("", String::from("simple input")))
        );
        assert_eq!(
            regex("/input with\\/embedded forward slash/"),
            Ok(("", String::from("input with/embedded forward slash")))
        );
        assert_eq!(
            regex("/input with \\w \\b \\s classes/"),
            Ok(("", String::from("input with \\w \\b \\s classes")))
        );
    }

    #[test]
    fn test_range() {
        let result = range_q("field:<4,<=5,>1,>=2");
        assert!(result.is_ok());
        let (remaining, query) = result.unwrap();
        assert_eq!(remaining, "");
        use serde_json;
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            serde_json::json!({"range":{"field":{"gte":2,"lte":5,"lt":4,"gt":1,"boost":0.0}}})
        );
    }

    #[test]
    fn test_simple_q_tree() {
        let result = expr("field:someterm");
        assert!(result.is_ok());
        let (remaining, query) = result.unwrap();
        assert_eq!(remaining, "");
        if let QueryTree::Simple(sq) = query {
            assert_eq!(
                serde_json::to_value(&sq).unwrap(),
                serde_json::json!({"term":{"field":"someterm"}})
            );
        } else {
            panic!("Expected simple query tree, got: {:?}", query)
        }
    }

    #[test]
    fn test_string_alone() {
        let result = parse_query("some_term", None);
        assert!(result.is_ok());
        let (remaining, query) = result.unwrap();
        assert_eq!(remaining, "");
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            serde_json::json!({"term":{"message":"some_term"}})
        );
    }

    #[test]
    fn test_quoted_alone() {
        let result = parse_query("\"quoted phrase\"", None);
        assert!(result.is_ok());
        let (remaining, query) = result.unwrap();
        assert_eq!(remaining, "");
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            serde_json::json!({"phrase":{"message":{"terms": ["quoted", "phrase"]}}})
        );
    }

    #[test]
    fn test_parse_complex_q_tree() {
        for inp in vec![
            "f:a AND f:b AND f:c",
            "f:a OR f:b OR f:c",
            "NOT f:a AND NOT f:b AND NOT f:c",
            "f:a OR (f:b AND NOT f:c)",
            "f:a OR f:b AND NOT f:c",
            "f:a AND f:b OR NOT f:c",
            "NOT f:a AND f:c OR f:d",
            "(f:a OR f:b) AND NOT f:c",
            "f:a OR f:b AND f:c OR f:d",
            "f:a AND f:b OR f:c AND f:d",
            "(f:a OR f:b) AND f:c OR f:d",
            "f:a AND (f:b OR f:c) AND f:d",
            "f:a AND f:b OR (f:c AND f:d)",
            "f:a OR (f:b AND f:c) OR f:d",
        ] {
            let result = parse_query(inp, None);
            println!("Parsed '{}' to {:?}", inp, result);
            assert!(result.is_ok());
            let (remaining, _query) = result.unwrap();
            assert_eq!(remaining, "");
        }
    }
}
