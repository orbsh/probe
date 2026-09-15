use probe_runtime::carrier::steel::introspect;

#[test]
fn steel_introspect_derives_and_merges() {
    let src = r##"
(on "add_to_cart" "user_id" (lambda (args) (hash "ok" #t)))
(on "order.*" "" (lambda (args) #t))

(define (interface_schema args)
  (hash "lifecycle" (hash "idle_ttl" "5m")))
"##;
    let schema = introspect(src).unwrap();
    println!("STEEL: {schema}");
    assert_eq!(schema["receives"]["add_to_cart"]["key"], "user_id");
    assert_eq!(schema["wildcard_receives"][0], "order.*");
    assert_eq!(schema["lifecycle"]["idle_ttl"], "5m");
}

#[test]
fn steel_introspect_derived_only() {
    let src = r##"
(on "remove_from_cart" "" (lambda (args) #t))
"##;
    let schema = introspect(src).unwrap();
    assert!(schema["receives"]["remove_from_cart"].is_object());
    assert!(schema["wildcard_receives"].as_array().unwrap().is_empty());
}
