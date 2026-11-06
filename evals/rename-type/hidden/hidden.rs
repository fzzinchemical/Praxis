use accounts::Account;
use accounts::model::Account as FromModel;
use accounts::report::summary;
use accounts::store::Store;

#[test]
fn renamed_type_works_through_every_module() {
    let mut store = Store::default();
    store.add(Account::new(1, "Ada"));
    let second: FromModel = Account::new(2, "Lin");
    store.add(second);
    assert_eq!(store.find(2).map(|a| a.name.as_str()), Some("Lin"));
    assert_eq!(summary(store.all()), "Ada, Lin");
}
