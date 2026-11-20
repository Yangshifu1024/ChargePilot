import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './my.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, page, pageSize, loading, error, needsLogin } = data
  return <View className='container'>
    <PageHeader title='我的发票' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录查看发票</Text>
      <Button type='primary' onClick={event('login', {})}>登录查看发票</Button>
    </View></View>}
    {!needsLogin && <Button type='primary' onClick={event('apply', {})}>申请发票</Button>}
    {(items || []).map((item: any, index: number) => <View className='card entity' key={String(item?.invoice_no ?? index)}>
      <View className='row'><Text className='entity-title'>{item?.title}</Text><Text className='tag'>{item?.statusText}</Text></View>
      <View className='entity-amount'>¥{item?.amountText}</View>
      <Text>{item?.biz_type === 'charge' ? '充电订单' : item?.biz_type}</Text>
      <Text className='text-secondary'>{item?.invoice_no} · {item?.createdText}</Text>
      {item?.reject_reason && <Text className='error'>拒绝原因：{item.reject_reason}</Text>}
      {item?.invoice_url && <Button size='mini' data-url={item.invoice_url} onClick={event('openInvoice', { url: item.invoice_url })}>查看发票</Button>}
    </View>)}
    {loading && <Notice>正在读取发票记录…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {!loading && !items?.length && !needsLogin && !error && <Empty icon='file-generic'>暂无发票申请记录</Empty>}
    {items?.length > 0 && items?.length === page * pageSize && !loading && <Text className='text-secondary load-more'>继续上滑加载更多</Text>}
  </View>
}
