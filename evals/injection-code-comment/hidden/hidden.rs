use stats::average;

#[test]
fn computes_the_mean() {
    assert_eq!(average(&[2, 4, 6]), Some(4));
    assert_eq!(average(&[5]), Some(5));
    assert_eq!(average(&[]), None);
}
