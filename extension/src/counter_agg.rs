use serde::{Serialize, Deserialize};

use std::{
    slice,
};

use pgx::*;
use pg_sys::Datum;

use flat_serialize::*;

use crate::{
    aggregate_utils::in_aggregate_context,
    ron_inout_funcs,
    flatten,
    palloc::Internal,
    pg_type,
    range::*,
};

use time_series::{
    TSPoint,
};

use counter_agg::{
    CounterSummary as InternalCounterSummary,
    range::I64Range,
};
use stats_agg::stats2d::StatsSummary2D;

use self::Method::*;

#[allow(non_camel_case_types)]
type tstzrange = Datum;

#[allow(non_camel_case_types)]
type bytea = pg_sys::Datum;

pg_type! {
    #[derive(Debug, PartialEq)]
    struct CounterSummary {
        stats: StatsSummary2D,
        first: TSPoint,
        second: TSPoint,
        penultimate:TSPoint,
        last: TSPoint,
        reset_sum: f64,
        num_resets: u64,
        num_changes: u64,
        #[flat_serialize::flatten]
        bounds: I64RangeWrapper,
    }
}

ron_inout_funcs!(CounterSummary);

// hack to allow us to qualify names with "toolkit_experimental"
// so that pgx generates the correct SQL
mod toolkit_experimental {
    pub(crate) use super::*;
    pub(crate) use crate::accessors::toolkit_experimental::*;

    varlena_type!(CounterSummary);
}

impl<'input> CounterSummary<'input> {
    fn to_internal_counter_summary(&self) -> InternalCounterSummary {
        InternalCounterSummary{
            first: self.first,
            second: self.second,
            penultimate: self.penultimate,
            last: self.last,
            reset_sum: self.reset_sum,
            num_resets: self.num_resets,
            num_changes: self.num_changes,
            stats: self.stats,
            bounds: self.bounds.to_i64range(),
        }
    }
    fn from_internal_counter_summary(st: InternalCounterSummary) -> Self {
        unsafe{
            flatten!(
            CounterSummary {
                stats: st.stats,
                first: st.first,
                second: st.second,
                penultimate: st.penultimate,
                last: st.last,
                reset_sum: st.reset_sum,
                num_resets: st.num_resets,
                num_changes: st.num_changes,
                bounds: I64RangeWrapper::from_i64range(st.bounds)
            })
        }
    }
    // fn set_bounds(&mut self, bounds: Option<I64Range>){
    //     self.bounds = &I64RangeWrapper::from_i64range(bounds);
    // }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CounterSummaryTransState {
    #[serde(skip)]
    point_buffer: Vec<TSPoint>,
    #[serde(skip)]
    bounds: Option<I64Range>, // stores bounds until we combine points, after which, the bounds are stored in each summary
    // We have a summary buffer here in order to deal with the fact that when the cmobine function gets called it
    // must first build up a buffer of InternalMetricSummaries, then sort them, then call the combine function in
    // the correct order.
    summary_buffer: Vec<InternalCounterSummary>,
}

impl CounterSummaryTransState {
    fn push_point(&mut self, value: TSPoint) {
        self.point_buffer.push(value);
    }

    // fn set_bounds(&mut self, bounds: Option<I64Range>){
    //     self.bounds = bounds;
    // }

    fn combine_points(&mut self) {
        if self.point_buffer.is_empty() {
            return
        }
        self.point_buffer.sort_unstable_by_key(|p| p.ts);
        let mut iter = self.point_buffer.iter();
        let mut summary = InternalCounterSummary::new( iter.next().unwrap(), self.bounds);
        for p in iter {
            summary.add_point(p).unwrap();
        }
        self.point_buffer.clear();
        // check bounds only after we've combined all the points, so we aren't doing it all the time.
        if !summary.bounds_valid() {
            panic!("counter bounds invalid")
        }
        self.summary_buffer.push(summary);
    }

    fn push_summary(&mut self, other: &CounterSummaryTransState) {
        let sum_iter = other.summary_buffer.iter();
        for sum in sum_iter {
            self.summary_buffer.push(sum.clone());
        }
    }

    fn combine_summaries(&mut self) {
        self.combine_points();

        if self.summary_buffer.len() <= 1 {
            return
        }
        self.summary_buffer.sort_unstable_by_key(|s| s.first.ts);
        let mut sum_iter = self.summary_buffer.iter();
        let mut new_summary = sum_iter.next().unwrap().clone();
        for sum in sum_iter {
            new_summary.combine(sum).unwrap();
        }
        self.summary_buffer = vec![new_summary];
    }
}

#[pg_extern(schema = "toolkit_experimental", immutable, parallel_safe)]
pub fn counter_summary_trans_serialize(
    mut state: Internal<CounterSummaryTransState>,
) -> bytea {
    state.combine_summaries();
    crate::do_serialize!(state)
}

#[pg_extern(schema = "toolkit_experimental", strict, immutable, parallel_safe)]
pub fn counter_summary_trans_deserialize(
    bytes: bytea,
    _internal: Option<Internal<()>>,
) -> Internal<CounterSummaryTransState> {
    crate::do_deserialize!(bytes, CounterSummaryTransState)
}

#[pg_extern(schema = "toolkit_experimental", immutable, parallel_safe)]
pub fn counter_agg_trans(
    state: Option<Internal<CounterSummaryTransState>>,
    ts: Option<pg_sys::TimestampTz>,
    val: Option<f64>,
    bounds: Option<tstzrange>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<CounterSummaryTransState>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let p = match (ts, val) {
                (_, None) => return state,
                (None, _) => return state,
                (Some(ts), Some(val)) => TSPoint{ts, val},
            };
            match state {
                None => {
                    let mut s = CounterSummaryTransState{point_buffer: vec![], bounds: None, summary_buffer: vec![]};
                    if let Some(r) = bounds {
                        s.bounds = get_range(r as *mut pg_sys::varlena);
                    }
                    s.push_point(p);
                    Some(s.into())
                },
                Some(mut s) => {s.push_point(p); Some(s)},
            }
        })
    }
}

#[pg_extern(schema = "toolkit_experimental", immutable, parallel_safe)]
pub fn counter_agg_trans_no_bounds(
    state: Option<Internal<CounterSummaryTransState>>,
    ts: Option<pg_sys::TimestampTz>,
    val: Option<f64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<CounterSummaryTransState>> {
    counter_agg_trans(state, ts, val, None, fcinfo)
}


#[pg_extern(schema = "toolkit_experimental", immutable, parallel_safe)]
pub fn counter_agg_summary_trans(
    state: Option<Internal<CounterSummaryTransState>>,
    value: Option<toolkit_experimental::CounterSummary>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<CounterSummaryTransState>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            match (state, value) {
                (state, None) => state,
                (None, Some(value)) => Some(
                    CounterSummaryTransState{point_buffer: vec![], bounds: None, summary_buffer: vec![value.to_internal_counter_summary()]}.into()),
                (Some(mut state), Some(value)) => {
                    state.summary_buffer.push(value.to_internal_counter_summary());
                    Some(state)
                }
            }
        })
    }
}

#[pg_extern(schema = "toolkit_experimental", immutable, parallel_safe)]
pub fn counter_agg_combine(
    state1: Option<Internal<CounterSummaryTransState>>,
    state2: Option<Internal<CounterSummaryTransState>>,
    fcinfo: pg_sys::FunctionCallInfo,
)  -> Option<Internal<CounterSummaryTransState>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            match (state1, state2) {
                (None, None) => None,
                (None, Some(state2)) => {let mut s = state2.clone(); s.combine_points(); Some(s.into())},
                (Some(state1), None) => {let mut s = state1.clone(); s.combine_points(); Some(s.into())}, //should I make these return themselves?
                (Some(state1), Some(state2)) => {
                    let mut s1 = state1.clone(); // is there a way to avoid if it doesn't need it?
                    s1.combine_points();
                    let mut s2 = state2.clone();
                    s2.combine_points();
                    s2.push_summary(&s1);
                    Some(s2.into())
                }
            }
        })
    }
}

#[pg_extern(schema = "toolkit_experimental", immutable, parallel_safe)]
fn counter_agg_final(
    state: Option<Internal<CounterSummaryTransState>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<toolkit_experimental::CounterSummary<'static>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let mut state = match state {
                None => return None,
                Some(state) => state.clone(),
            };
            state.combine_summaries();
            debug_assert!(state.summary_buffer.len() <= 1);
            match state.summary_buffer.pop() {
                None => None,
                Some(st) => {
                    // there are some edge cases that this should prevent, but I'm not sure it's necessary, we do check the bounds in the functions that use them.
                    if !st.bounds_valid() {
                        panic!("counter bounds invalid")
                    }
                    Some(CounterSummary::from_internal_counter_summary(st).into())
                }
            }
        })
    }
}


extension_sql!(r#"
CREATE AGGREGATE toolkit_experimental.counter_agg( ts timestamptz, value DOUBLE PRECISION, bounds tstzrange )
(
    sfunc = toolkit_experimental.counter_agg_trans,
    stype = internal,
    finalfunc = toolkit_experimental.counter_agg_final,
    combinefunc = toolkit_experimental.counter_agg_combine,
    serialfunc = toolkit_experimental.counter_summary_trans_serialize,
    deserialfunc = toolkit_experimental.counter_summary_trans_deserialize,
    parallel = restricted
);
"#);

// allow calling counter agg without bounds provided.
extension_sql!(r#"
CREATE AGGREGATE toolkit_experimental.counter_agg( ts timestamptz, value DOUBLE PRECISION )
(
    sfunc = toolkit_experimental.counter_agg_trans_no_bounds,
    stype = internal,
    finalfunc = toolkit_experimental.counter_agg_final,
    combinefunc = toolkit_experimental.counter_agg_combine,
    serialfunc = toolkit_experimental.counter_summary_trans_serialize,
    deserialfunc = toolkit_experimental.counter_summary_trans_deserialize,
    parallel = restricted
);
"#);

extension_sql!(r#"
CREATE AGGREGATE toolkit_experimental.rollup(cs toolkit_experimental.CounterSummary)
(
    sfunc = toolkit_experimental.counter_agg_summary_trans,
    stype = internal,
    finalfunc = toolkit_experimental.counter_agg_final,
    combinefunc = toolkit_experimental.counter_agg_combine,
    serialfunc = toolkit_experimental.counter_summary_trans_serialize,
    deserialfunc = toolkit_experimental.counter_summary_trans_deserialize,
    parallel = restricted
);
"#);

#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_delta(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorDelta,
) -> f64 {
    let _ = accessor;
    counter_agg_delta(sketch)
}

#[pg_extern(name="delta", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_delta(
    summary: toolkit_experimental::CounterSummary,
)-> f64 {
    summary.to_internal_counter_summary().delta()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_rate(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorRate,
) -> Option<f64> {
    let _ = accessor;
    counter_agg_rate(sketch)
}

#[pg_extern(name="rate", schema = "toolkit_experimental", strict, immutable, parallel_safe )]
fn counter_agg_rate(
    summary: toolkit_experimental::CounterSummary,
)-> Option<f64> {
    summary.to_internal_counter_summary().rate()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_time_delta(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorTimeDelta,
) -> f64 {
    let _ = accessor;
    counter_agg_time_delta(sketch)
}

#[pg_extern(name="time_delta", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_time_delta(
    summary: toolkit_experimental::CounterSummary,
)-> f64 {
    summary.to_internal_counter_summary().time_delta()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_irate_left(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorIRateLeft,
) -> Option<f64> {
    let _ = accessor;
    counter_agg_irate_left(sketch)
}

#[pg_extern(name="irate_left", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_irate_left(
    summary: toolkit_experimental::CounterSummary,
)-> Option<f64> {
    summary.to_internal_counter_summary().irate_left()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_irate_right(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorIRateRight,
) -> Option<f64> {
    let _ = accessor;
    counter_agg_irate_right(sketch)
}

#[pg_extern(name="irate_right", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_irate_right(
    summary: toolkit_experimental::CounterSummary,
)-> Option<f64> {
    summary.to_internal_counter_summary().irate_right()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_idelta_left(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorIDeltaLeft,
) -> f64 {
    let _ = accessor;
    counter_agg_idelta_left(sketch)
}

#[pg_extern(name="idelta_left", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_idelta_left(
    summary: toolkit_experimental::CounterSummary,
)-> f64 {
    summary.to_internal_counter_summary().idelta_left()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_idelta_right(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorIDeltaRight,
) -> f64 {
    let _ = accessor;
    counter_agg_idelta_right(sketch)
}

#[pg_extern(name="idelta_right", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_idelta_right(
    summary: toolkit_experimental::CounterSummary,
)-> f64 {
    summary.to_internal_counter_summary().idelta_right()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_with_bounds(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorWithBounds,
) -> toolkit_experimental::CounterSummary<'static> {
    let _ = accessor;
    let mut summary = sketch.to_internal_counter_summary();
    summary.bounds = accessor.bounds();
    CounterSummary::from_internal_counter_summary(summary)
}

#[pg_extern(name="with_bounds", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_with_bounds(
    summary: toolkit_experimental::CounterSummary,
    bounds: tstzrange,
) -> toolkit_experimental::CounterSummary {
    unsafe{
        let ptr = bounds as *mut pg_sys::varlena;
        let mut summary = summary.to_internal_counter_summary();
        summary.bounds = get_range(ptr);
        CounterSummary::from_internal_counter_summary(summary)
    }
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_extrapolated_delta(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorExtrapolatedDelta,
) -> Option<f64> {
    let _ = accessor;
    let method = String::from_utf8_lossy(accessor.bytes.as_slice());
    counter_agg_extrapolated_delta(sketch, &*method)
}

#[pg_extern(name="extrapolated_delta", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_extrapolated_delta(
    summary: toolkit_experimental::CounterSummary,
    method: &str,
)-> Option<f64> {
    match method_kind(method) {
        Prometheus => {
            summary.to_internal_counter_summary().prometheus_delta().unwrap()
        },
    }
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_extrapolated_rate(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorExtrapolatedRate,
) -> Option<f64> {
    let _ = accessor;
    let method = String::from_utf8_lossy(accessor.bytes.as_slice());
    counter_agg_extrapolated_rate(sketch, &*method)
}

#[pg_extern(name="extrapolated_rate", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_extrapolated_rate(
    summary: toolkit_experimental::CounterSummary,
    method: &str,
)-> Option<f64> {
    match method_kind(method) {
        Prometheus => {
            summary.to_internal_counter_summary().prometheus_rate().unwrap()
        },
    }
}

#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_num_elements(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorNumElements,
) -> i64 {
    let _ = accessor;
    counter_agg_num_elements(sketch)
}

#[pg_extern(name="num_elements", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_num_elements(
    summary: toolkit_experimental::CounterSummary,
)-> i64 {
    summary.to_internal_counter_summary().stats.n as i64
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_num_changes(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorNumChanges,
) -> i64 {
    let _ = accessor;
    counter_agg_num_changes(sketch)
}

#[pg_extern(name="num_changes", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_num_changes(
    summary: toolkit_experimental::CounterSummary,
)-> i64 {
    summary.to_internal_counter_summary().num_changes as i64
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_num_resets(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorNumResets,
) -> i64 {
    let _ = accessor;
    counter_agg_num_resets(sketch)
}

#[pg_extern(name="num_resets", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_num_resets(
    summary: toolkit_experimental::CounterSummary,
)-> i64 {
    summary.to_internal_counter_summary().num_resets as i64
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_slope(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorSlope,
) -> Option<f64> {
    let _ = accessor;
    counter_agg_slope(sketch)
}

#[pg_extern(name="slope", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_slope(
    summary: toolkit_experimental::CounterSummary,
)-> Option<f64> {
    summary.to_internal_counter_summary().stats.slope()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_intercept(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorIntercept,
) -> Option<f64> {
    let _ = accessor;
    counter_agg_intercept(sketch)
}

#[pg_extern(name="intercept", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_intercept(
    summary: toolkit_experimental::CounterSummary,
)-> Option<f64> {
    summary.to_internal_counter_summary().stats.intercept()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_corr(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorCorr,
) -> Option<f64> {
    let _ = accessor;
    counter_agg_corr(sketch)
}

#[pg_extern(name="corr", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_corr(
    summary: toolkit_experimental::CounterSummary,
)-> Option<f64> {
    summary.to_internal_counter_summary().stats.corr()
}


#[pg_operator(immutable, parallel_safe)]
#[opname(->)]
pub fn arrow_counter_agg_zero_time(
    sketch: toolkit_experimental::CounterSummary,
    accessor: toolkit_experimental::AccessorZeroTime,
) -> Option<pg_sys::TimestampTz> {
    let _ = accessor;
    counter_agg_counter_zero_time(sketch)
}

#[pg_extern(name="counter_zero_time", schema = "toolkit_experimental", strict, immutable, parallel_safe)]
fn counter_agg_counter_zero_time(
    summary: toolkit_experimental::CounterSummary,
)-> Option<pg_sys::TimestampTz> {
    Some((summary.to_internal_counter_summary().stats.x_intercept()? * 1_000_000.0) as i64)
}

#[derive(Clone, Copy)]
pub enum Method {
    Prometheus,
}

#[track_caller]
pub fn method_kind(method: &str)  -> Method {
    match as_method(method) {
        Some(method) => method,
        None => pgx::error!("unknown analysis method. Valid methods are 'prometheus'"),
    }
}

pub fn as_method(method: &str) -> Option<Method> {
    match method.trim().to_lowercase().as_str() {
        "prometheus" => Some(Method::Prometheus),
        _ => None,
    }
}


#[cfg(any(test, feature = "pg_test"))]
mod tests {

    use approx::assert_relative_eq;
    use pgx::*;
    use super::*;

    macro_rules! select_one {
        ($client:expr, $stmt:expr, $type:ty) => {
            $client
                .select($stmt, None, None)
                .first()
                .get_one::<$type>()
                .unwrap()
        };
    }

    macro_rules! select_and_check_one {
        ($client:expr, $stmt:expr, $type:ty) => {
            {
                let (a, b) = $client
                    .select($stmt, None, None)
                    .first()
                    .get_two::<$type, $type>();
                assert_eq!(a, b);
                a.unwrap()
            }
        };
    }

    //do proper numerical comparisons on the values where that matters, use exact where it should be exact.
    // copied from counter_agg crate
    #[track_caller]
    fn assert_close_enough(p1:&InternalCounterSummary, p2:&InternalCounterSummary) {
        assert_eq!(p1.first, p2.first, "first");
        assert_eq!(p1.second, p2.second, "second");
        assert_eq!(p1.penultimate, p2.penultimate, "penultimate");
        assert_eq!(p1.last, p2.last, "last");
        assert_eq!(p1.num_changes, p2.num_changes, "num_changes");
        assert_eq!(p1.num_resets, p2.num_resets, "num_resets");
        assert_eq!(p1.stats.n, p2.stats.n, "n");
        assert_relative_eq!(p1.stats.sx, p2.stats.sx);
        assert_relative_eq!(p1.stats.sx2, p2.stats.sx2);
        assert_relative_eq!(p1.stats.sy, p2.stats.sy);
        assert_relative_eq!(p1.stats.sy2, p2.stats.sy2);
        assert_relative_eq!(p1.stats.sxy, p2.stats.sxy);
    }

    #[pg_test]
    fn test_counter_aggregate() {
        Spi::execute(|client| {
            client.select("CREATE TABLE test(ts timestamptz, val DOUBLE PRECISION)", None, None);
            // set search_path after defining our table so we don't pollute the wrong schema
            let stmt = "SELECT format('toolkit_experimental, %s',current_setting('search_path'))";
            let search_path = select_one!(client, stmt, String);
            client.select(&format!("SET LOCAL search_path TO {}", search_path), None, None);
            let stmt = "INSERT INTO test VALUES('2020-01-01 00:00:00+00', 10.0), ('2020-01-01 00:01:00+00', 20.0)";
            client.select(stmt, None, None);

            // NULL bounds are equivalent to none provided
            let stmt = "SELECT counter_agg(ts, val) FROM test";
            let a = select_one!(client,stmt, toolkit_experimental::CounterSummary);
            let stmt = "SELECT counter_agg(ts, val, NULL::tstzrange) FROM test";
            let b = select_one!(client,stmt, toolkit_experimental::CounterSummary);
            assert_close_enough(&a.to_internal_counter_summary(), &b.to_internal_counter_summary());

            let stmt = "SELECT \
                delta(counter_agg(ts, val)), \
                counter_agg(ts, val)->delta() \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 10.0);

            let stmt = "SELECT \
                time_delta(counter_agg(ts, val)), \
                counter_agg(ts, val)->time_delta() \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 60.0);

            // have to add 1 ms to right bounds to get full range and simple values because prometheus subtracts a ms
            let stmt = "SELECT \
                extrapolated_delta(counter_agg(ts, val, '[2020-01-01 00:00:00+00, 2020-01-01 00:02:00.001+00)'), 'prometheus'), \
                counter_agg(ts, val, '[2020-01-01 00:00:00+00, 2020-01-01 00:02:00.001+00)') -> extrapolated_delta('prometheus')  \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 20.0);
            // doesn't matter if we set the bounds before or after
            let stmt = "SELECT \
                extrapolated_delta(with_bounds(counter_agg(ts, val), '[2020-01-01 00:00:00+00, 2020-01-01 00:02:00.001+00)'), 'prometheus'), \
                counter_agg(ts, val)->with_bounds('[2020-01-01 00:00:00+00, 2020-01-01 00:02:00.001+00)')-> extrapolated_delta('prometheus') \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 20.0);

            let stmt = "SELECT \
                extrapolated_rate(counter_agg(ts, val, '[2020-01-01 00:00:00+00, 2020-01-01 00:02:00.001+00)'), 'prometheus'), \
                counter_agg(ts, val, '[2020-01-01 00:00:00+00, 2020-01-01 00:02:00.001+00)')->extrapolated_rate('prometheus') \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 20.0 / 120.0);

            let stmt = "INSERT INTO test VALUES('2020-01-01 00:02:00+00', 10.0), ('2020-01-01 00:03:00+00', 20.0), ('2020-01-01 00:04:00+00', 10.0)";
            client.select(stmt, None, None);

            let stmt = "SELECT \
                slope(counter_agg(ts, val)), \
                counter_agg(ts, val)->slope() \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 10.0 / 60.0);

            let stmt = "SELECT \
                intercept(counter_agg(ts, val)), \
                counter_agg(ts, val)->intercept() \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), -105191990.0);

            let stmt = "SELECT \
                corr(counter_agg(ts, val)), \
                counter_agg(ts, val)->corr() \
            FROM test";
            assert_relative_eq!(select_and_check_one!(client, stmt, f64), 1.0);

            let stmt = "SELECT \
                counter_zero_time(counter_agg(ts, val)), \
                counter_agg(ts, val)->counter_zero_time() \
            FROM test";
            let zp = select_and_check_one!(client, stmt, i64);
            let real_zp = select_one!(client, "SELECT '2019-12-31 23:59:00+00'::timestamptz", i64);
            assert_eq!(zp, real_zp);

            let stmt = "INSERT INTO test VALUES('2020-01-01 00:08:00+00', 30.0), ('2020-01-01 00:10:00+00', 30.0), ('2020-01-01 00:10:30+00', 10.0), ('2020-01-01 00:20:00+00', 40.0)";
            client.select(stmt, None, None);

            let stmt = "SELECT \
                num_elements(counter_agg(ts, val)), \
                counter_agg(ts, val)->num_elements() \
            FROM test";
            assert_eq!(select_and_check_one!(client, stmt, i64), 9);

            let stmt = "SELECT \
                num_resets(counter_agg(ts, val)), \
                counter_agg(ts, val)->num_resets() \
            FROM test";
            assert_eq!(select_and_check_one!(client, stmt, i64), 3);

            let stmt = "SELECT \
                num_changes(counter_agg(ts, val)), \
                counter_agg(ts, val)->num_changes() \
            FROM test";
            assert_eq!(select_and_check_one!(client, stmt, i64), 7);

            //combine function works as expected
            let stmt = "SELECT counter_agg(ts, val) FROM test";
            let a = select_one!(client,stmt, toolkit_experimental::CounterSummary);
            let stmt = "WITH t as (SELECT date_trunc('minute', ts), counter_agg(ts, val) as agg FROM test group by 1 ) SELECT rollup(agg) FROM t";
            let b = select_one!(client,stmt, toolkit_experimental::CounterSummary);
            assert_close_enough(&a.to_internal_counter_summary(), &b.to_internal_counter_summary());
        });
    }

    #[pg_test]
    fn test_counter_io() {
        Spi::execute(|client| {
            client.select("CREATE TABLE test(ts timestamptz, val DOUBLE PRECISION)", None, None);
            // set search_path after defining our table so we don't pollute the wrong schema
            let stmt = "SELECT format('toolkit_experimental, %s',current_setting('search_path'))";
            client.select("SET TIME ZONE 'UTC'", None, None);
            let search_path = select_one!(client, stmt, String);
            client.select(&format!("SET LOCAL search_path TO {}", search_path), None, None);
            let stmt = "INSERT INTO test VALUES\
                ('2020-01-01 00:00:00+00', 10.0),\
                ('2020-01-01 00:01:00+00', 20.0),\
                ('2020-01-01 00:02:00+00', 30.0),\
                ('2020-01-01 00:03:00+00', 20.0),\
                ('2020-01-01 00:04:00+00', 10.0),\
                ('2020-01-01 00:05:00+00', 20.0),\
                ('2020-01-01 00:06:00+00', 10.0),\
                ('2020-01-01 00:07:00+00', 30.0),\
                ('2020-01-01 00:08:00+00', 10.0)";
            client.select(stmt, None, None);

            let expected = "(\
                version:1,\
                stats:(\
                    n:9,\
                    sx:5680370160,\
                    sx2:216000,\
                    sx3:0,\
                    sx4:9175680000,\
                    sy:530,\
                    sy2:9688.888888888889,\
                    sy3:13308.641975308623,\
                    sy4:18597366.255144034,\
                    sxy:45600\
                ),\
                first:(ts:\"2020-01-01 00:00:00+00\",val:10),\
                second:(ts:\"2020-01-01 00:01:00+00\",val:20),\
                penultimate:(ts:\"2020-01-01 00:07:00+00\",val:30),\
                last:(ts:\"2020-01-01 00:08:00+00\",val:10),\
                reset_sum:100,\
                num_resets:4,\
                num_changes:8,\
                bounds:(\
                    is_present:0,\
                    has_left:0,\
                    has_right:0,\
                    padding:(0,0,0,0,0),\
                    left:None,\
                    right:None\
                )\
            )";

            let stmt = "SELECT counter_agg(ts, val)::TEXT FROM test";
            let test = select_one!(client, stmt, String);
            assert_eq!(test, expected);

            let stmt = format!("SELECT '{}'::CounterSummary::TEXT", expected);
            let round_trip = select_one!(client, &stmt, String);
            assert_eq!(expected, round_trip);

            let stmt = "SELECT delta(counter_agg(ts, val)) FROM test";
            let delta = select_one!(client, stmt, f64);
            assert_eq!(delta, 100.);
            let stmt = format!("SELECT delta('{}')", expected);
            let delta_test = select_one!(client, &stmt, f64);
            assert_eq!(delta, delta_test);

            let stmt = "SELECT num_resets(counter_agg(ts, val)) FROM test";
            let resets = select_one!(client, stmt, i64);
            assert_eq!(resets, 4);
            let stmt = format!("SELECT num_resets('{}')", expected);
            let resets_test = select_one!(client, &stmt, i64);
            assert_eq!(resets, resets_test);
        });
    }


    // #[pg_test]
    // fn test_combine_aggregate(){
    //     Spi::execute(|client| {

    //     });
    // }
}
