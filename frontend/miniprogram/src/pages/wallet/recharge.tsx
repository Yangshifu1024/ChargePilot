import { Button, Input, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Notice, PageHeader } from '../../runtime/ui'
import controller from './recharge.controller'

const PRESETS = ['10', '20', '30', '50', '100', '200']

export default function PageView() {
  const { data, event } = useController(controller)
  const { amount, items, hasMore, loading, paying, pending, error, notice, needsLogin } = data
  return <View className='container'>
    <PageHeader title='钱包充值' subtitle='到账以服务端支付结果为准' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后充值</Text>
      <Button type='primary' onClick={event('login', {})}>微信登录</Button>
    </View></View>}
    {!needsLogin && <View className='card'>
      <Text className='eyebrow'>充值金额</Text>
      <View className='money-large'>{amount ? `¥${amount}` : '输入金额'}</View>
      <View className='quick-amounts'>{PRESETS.map(value => <Button key={value} className={String(amount) === value ? 'is-selected' : ''} disabled={pending || paying} onClick={() => event('inputAmount')({ detail: { value } })}>{value} 元</Button>)}</View>
      <Text className='field-label'>自定义金额（元）</Text>
      <Input type='digit' value={amount} disabled={pending || paying} onInput={event('inputAmount', {})} placeholder='请输入充值金额' />
      <Button type='primary' onClick={event('submit', {})} loading={paying} disabled={paying || loading}>{pending ? '继续原充值单 / 核实结果' : '微信支付充值'}</Button>
      <Text className='text-secondary'>取消或网络中断后可继续同一笔充值。</Text>
    </View>}
    {error && <Notice tone='error'>{error}</Notice>}
    {notice && <View className='banner banner-ok'>{notice}</View>}
    <Button onClick={event('refresh', {})} disabled={paying || loading}>刷新充值记录</Button>
    {(items || []).map((item: any, index: number) => <View className='card entity' key={String(item?.request_id ?? index)}>
      <Text className='text-secondary'>{item?.pay_order_no}</Text>
      <View className='entity-amount'>¥{item?.amountText}</View>
      <Text>{item?.statusText}</Text>
      {item?.can_pay && <Button onClick={event('resume', { id: item?.request_id })} data-id={item?.request_id} disabled={paying || pending}>继续支付</Button>}
    </View>)}
    {hasMore && <Button onClick={event('more', {})} disabled={loading || paying}>加载更多</Button>}
  </View>
}
