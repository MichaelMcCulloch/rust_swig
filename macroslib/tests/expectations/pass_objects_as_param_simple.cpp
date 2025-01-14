"typedef struct FooOpaque FooOpaque;";

"void TestPassObjectsAsParams_f1(const TestPassObjectsAsParamsOpaque * const self, const FooOpaque * a_0);";
"void TestPassObjectsAsParams_f2(const TestPassObjectsAsParamsOpaque * const self, FooOpaque * a_0);";
"void TestPassObjectsAsParams_f3(const TestPassObjectsAsParamsOpaque * const self, FooOpaque * a_0);";
"void TestPassObjectsAsParams_f3_a(const TestPassObjectsAsParamsOpaque * const self, BooOpaque * a_0);";
"void TestPassObjectsAsParams_f4(const FooOpaque * a_0);";
"void TestPassObjectsAsParams_f5(FooOpaque * a_0);";

"void f1(const Foo & a_0) const  noexcept";
"void f2(Foo a_0) const  noexcept";
"void f3(Foo & a_0) const  noexcept";
"void f3_a(Boo & a_0) const  noexcept";
"static void f4(const Foo & a_0) noexcept";
"static void f5(Foo a_0) noexcept";

r#"inline void TestPassObjectsAsParamsWrapper<OWN_DATA>::f1(const Foo & a_0) const  noexcept
    {
        TestPassObjectsAsParams_f1(this->self_, static_cast<const FooOpaque *>(a_0));
    }"#;
    
r#"inline void TestPassObjectsAsParamsWrapper<OWN_DATA>::f2(Foo a_0) const  noexcept
    {
        TestPassObjectsAsParams_f2(this->self_, a_0.release());
    }"#;
    
    r#"inline void TestPassObjectsAsParamsWrapper<OWN_DATA>::f3(Foo & a_0) const  noexcept
    {
        TestPassObjectsAsParams_f3(this->self_, static_cast<FooOpaque *>(a_0));
    }"#;

r#"template<bool OWN_DATA>
    inline void TestPassObjectsAsParamsWrapper<OWN_DATA>::f3_a(Boo & a_0) const  noexcept
    {
        TestPassObjectsAsParams_f3_a(this->self_, static_cast<BooOpaque *>(a_0));
    }"#;

r#"template<bool OWN_DATA>
    inline void TestPassObjectsAsParamsWrapper<OWN_DATA>::f4(const Foo & a_0) noexcept
    {
        TestPassObjectsAsParams_f4(static_cast<const FooOpaque *>(a_0));
    }"#;

r#"template<bool OWN_DATA>
    inline void TestPassObjectsAsParamsWrapper<OWN_DATA>::f5(Foo a_0) noexcept
    {
        TestPassObjectsAsParams_f5(a_0.release());
    }"#;
