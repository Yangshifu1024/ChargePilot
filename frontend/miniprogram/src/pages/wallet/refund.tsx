import { Button, Input, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './refund.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, amount, reason, loading, refunding, pendingRetry, error, notice, needsLogin } = data
  return <View className='container'>
    <PageHeader title='钱包退款' subtitle='按原充值支付单退回' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录查看退款</Text>
      <Button type='primary' onClick={event('login', {})}>登录查看退款</Button>
    </View></View>}
    {!needsLogin && <View className='card'>
      <Text className='heading'>申请原路退款</Text>
      <Text className='field-label'>退款金额（元）</Text>
      <Input aria-label='退款金额' type='digit' placeholder='金额（元）' value={amount} onInput={event('inputAmount', {})} disabled={pendingRetry || refunding} />
      <Text className='field-label'>退款原因（可选）</Text>
      <Input aria-label='退款原因' placeholder='退款原因（可选）' maxlength={255} value={reason} onInput={event('inputReason', {})} disabled={pendingRetry || refunding} />
      {pendingRetry && <View className='banner banner-warn'>上次申请结果尚未确认，重试将沿用同一申请，不会重复预留余额。</View>}
      <Button type='primary' onClick={event('submit', {})} loading={refunding} disabled={loading || refunding}>{pendingRetry ? '重试同一申请' : '申请退款'}</Button>
    </View>}
    {notice && <View className='banner banner-ok'>{notice}</View>}
    {error && <Notice tone='error' retry={event('load', {})}>{error}</Notice>}
    {loading && <Notice>正在读取退款进度…</Notice>}
    {(items || []).map((item: any, index: number) => <View className='card entity' key={String(item?.request_id ?? index)}>
      <View className='row'><Text className='entity-amount'>{item?.amountText}</Text><Text className='tag'>{item?.statusText}</Text></View>
      <Text>已退款 {item?.refundedText}</Text>
      <Text className='text-secondary'>{item?.created_at}</Text>
      {item?.review && <Text>审核意见：{item.review.comment}</Text>}
      {(item?.refund_orders || []).map((part: any, partIndex: number) => <View className='kv' key={String(part?.refund_no ?? partIndex)}>
        <View><Text>{part?.amountText} · {part?.statusText}</Text><Text className='text-secondary'>{part?.refund_no}</Text>{part?.failure_reason && <Text className='error'>{part.failure_reason}</Text>}</View>
      </View>)}
    </View>)}
    {!loading && !items?.length && !needsLogin && <Empty icon='reload'>暂无退款申请</Empty>}
  </View>
}
