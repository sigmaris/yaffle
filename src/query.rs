use nom::branch::alt;
use nom::bytes::complete::{escaped, is_not, tag, take, take_until, take_while1, take_while_m_n};
use nom::character::complete::{char, digit1, one_of};
use nom::character::{is_alphanumeric, is_digit, is_space};
use nom::combinator::{map, map_res, opt, recognize, rest, value};
use nom::error::ParseError;
use nom::multi::separated_list1;
use nom::sequence::{delimited, preceded, separated_pair, terminated, tuple};
use nom::IResult;
use toshi::{ExactTerm, FuzzyQuery, PhraseQuery, Query, RangeQuery, RegexQuery, TermPair};

fn identifier(input: &str) -> IResult<&str, &str> {
    take_while1(|ch: char| ch.is_ascii_lowercase() || ch == '_')(input)
}

fn basic_term(input: &str) -> IResult<&str, &str> {
    take_while1(|ch: char| ch.is_alphanumeric() || ch == '_')(input)
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

enum Bound<'a> {
    Greater { than: &'a str, or_equal: bool },
    Less { than: &'a str, or_equal: bool },
}

fn range_bound(input: &str) -> IResult<&str, Bound> {
    alt((
        map(preceded(tag(">="), basic_term), |term| Bound::Greater {
            than: term,
            or_equal: true,
        }),
        map(preceded(tag(">"), basic_term), |term| Bound::Greater {
            than: term,
            or_equal: false,
        }),
        map(preceded(tag("<="), basic_term), |term| Bound::Less {
            than: term,
            or_equal: true,
        }),
        map(preceded(tag("<"), basic_term), |term| Bound::Less {
            than: term,
            or_equal: false,
        }),
    ))(input)
}

fn fuzzy_term_q(input: &str) -> IResult<&str, Query> {
    map_res(
        separated_pair(
            identifier,
            tag(":"),
            preceded(
                tag("~"),
                tuple((opt(terminated(digit1, tag("~"))), basic_term)),
            ),
        ),
        |(field_name, (distance_opt, term))| {
            let distance = if let Some(dist_str) = distance_opt {
                u8::from_str_radix(dist_str, 10)?
            } else {
                3u8
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
        separated_pair(identifier, tag(":"), basic_term),
        |(field_name, term)| Query::Exact(ExactTerm::with_term(field_name, term)),
    )(input)
}

fn phrase_q(input: &str) -> IResult<&str, Query> {
    map(
        separated_pair(identifier, tag(":"), quoted),
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
        separated_pair(identifier, tag(":"), regex),
        |(field_name, regex_string)| {
            Query::Regex(RegexQuery::from_str(field_name.to_string(), regex_string))
        },
    )(input)
}

fn range_q(input: &str) -> IResult<&str, Query> {
    map(
        separated_pair(identifier, tag(":"), separated_list1(tag(","), range_bound)),
        |(field_name, bounds)| {
            let mut builder = RangeQuery::builder().for_field(field_name);
            for bound in bounds {
                match bound {
                    Bound::Greater {
                        than,
                        or_equal: true,
                    } => builder = builder.gte(than),
                    Bound::Greater {
                        than,
                        or_equal: false,
                    } => builder = builder.gt(than),
                    Bound::Less {
                        than,
                        or_equal: true,
                    } => builder = builder.lte(than),
                    Bound::Less {
                        than,
                        or_equal: false,
                    } => builder = builder.lt(than),
                }
            }
            builder.build()
        },
    )(input)
}

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
        serde_json::json!({"range":{"field":{"gte":"2","lte":"5","lt":"4","gt":"1","boost":0.0}}})
    );
}
