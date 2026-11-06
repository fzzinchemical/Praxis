use csvsum::total_amount;

#[test]
fn tolerates_spaces_around_values() {
    assert_eq!(total_amount("id,amount\n1,  5 \n2,7\n"), Ok(12));
}

#[test]
fn still_rejects_non_numbers() {
    assert!(total_amount("id,amount\n1,x\n").is_err());
}
