use super::boxed_delegate_field;

#[tokio::test]
async fn an_owned_delegate_task_has_a_pointer_sized_spawn_frame() {
    let payload = [7_u8; 32 * 1024];
    let task = boxed_delegate_field(|| async move {
        tokio::task::yield_now().await;
        payload.iter().map(|byte| usize::from(*byte)).sum::<usize>()
    });
    assert!(std::mem::size_of_val(&task) <= 2 * std::mem::size_of::<usize>());
    assert_eq!(tokio::spawn(task).await.expect("owned task completes"), 7 * 32 * 1024);
}
